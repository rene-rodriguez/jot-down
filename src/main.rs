// Consumed by the embedding worker (task AI-1c); allow until that lands.
#[cfg(feature = "ai")]
#[allow(dead_code, unused_imports)]
mod ai;
mod app;
mod config;
mod logging;
mod models;
mod notes;
mod notes_io;
mod storage;
mod tui;
mod workers;

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use tokio::sync::{mpsc, oneshot};
use tracing::info;

use app::actions::{self, AppAction};
use app::commands::{self, CommandId};
use app::state::{AppState, AppView, EditKind, PreviewLink, SearchKind};
use config::Settings;
use models::note::NoteSummary;
use models::note::UpdateNoteInput;
use storage::remote::PostgresStorage;
use storage::ConflictResolution;
use storage::SqliteStorage;
use tui::events::{self, AppEvent};
use tui::layout;
use tui::markdown;
use uuid::Uuid;
use workers::{AskEvent, PersistenceCommand, PersistenceEvent, SyncCommand, SyncEvent};

/// Application entry point.
///
/// Errors during startup are printed to stderr before the process exits.
/// Runtime errors are shown in the TUI status bar.
fn main() {
    // Install panic hook before anything else
    logging::install_panic_hook();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    if let Err(e) = rt.block_on(async_main()) {
        // Ensure terminal is restored before printing the error
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

async fn async_main() -> Result<()> {
    // Whether this is a first run (no config file yet) — used to show the
    // welcome / setup screen. Checked before anything writes a config.
    let first_run = !Settings::config_exists();

    // Load configuration
    let settings = Settings::load().map_err(|e| {
        anyhow::anyhow!("Failed to load configuration: {}. Create ~/.config/jot-down/config.toml or check the file format.", e)
    })?;
    settings.ensure_dirs()?;

    // `jot-down doctor` — print diagnostics and exit (no TUI).
    if std::env::args().skip(1).any(|arg| arg == "doctor") {
        return run_doctor(&settings).await;
    }

    // Initialize logging (after config so we know the data dir)
    let log_dir = settings.data_dir.join("logs");
    let _ = logging::init(Some(&log_dir));
    info!("Jot starting up");
    info!("Data directory: {:?}", settings.data_dir);
    if first_run {
        info!("No config file found — showing first-run setup");
    }

    // Connect to SQLite and run migrations
    let storage = SqliteStorage::connect_with_ai(&settings.db_path, settings.ai.enabled)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open database at {:?}: {}", settings.db_path, e))?;
    info!("Database initialized at: {:?}", settings.db_path);

    // Keep the embedding index in step with the active model. If the embedder
    // changed since the index was last built (different id or vector width),
    // every stored vector is stale, so rebuild from scratch. Otherwise just
    // backfill notes that were never embedded or have changed. Either way the
    // background worker drains the resulting queue.
    #[cfg(feature = "ai")]
    if storage.ai_available() {
        use crate::ai::Embedder;
        let embedder = crate::ai::active_embedder();
        let (id, dims) = (embedder.id(), embedder.dimensions());

        let changed = storage
            .embedding_model_changed(id, dims)
            .await
            .unwrap_or(false);

        let result = if changed {
            storage
                .reindex_embeddings(id, dims)
                .await
                .map(|n| (n, true))
        } else {
            storage.enqueue_all_stale_embeddings().await.map(|n| (n, false))
        };

        match result {
            Ok((n, true)) => info!("Embedding model is '{id}' — reindexing {n} note(s)"),
            Ok((n, false)) if n > 0 => info!("Enqueued {n} note(s) for embedding"),
            Ok(_) => {}
            Err(e) => tracing::warn!("Failed to prepare embedding index: {:?}", e),
        }
    }

    // Optionally connect to PostgreSQL for sync
    let postgres = if settings.sync.enabled {
        match settings.sync.database_url.as_ref() {
            Some(url) => {
                let device_id = storage.get_or_create_device_id().await?;
                info!("Device ID: {}", device_id);
                match PostgresStorage::connect(url, device_id).await {
                    Ok(pg) => {
                        info!("PostgreSQL sync enabled");
                        Some(pg)
                    }
                    Err(e) => {
                        tracing::warn!("PostgreSQL connection failed (sync disabled): {:?}", e);
                        None
                    }
                }
            }
            None => {
                tracing::warn!(
                    "Sync enabled but no database URL configured (set JOT_DATABASE_URL)"
                );
                None
            }
        }
    } else {
        None
    };

    // Run the TUI application
    run_app(storage, postgres, settings, first_run).await?;

    Ok(())
}

/// Print a `jot-down doctor` diagnostic report, then exit (no TUI).
async fn run_doctor(settings: &Settings) -> Result<()> {
    println!("jot-down doctor");
    println!("==========\n");

    println!("Storage");
    println!(
        "  data dir:    {}  [{}]",
        settings.data_dir.display(),
        if settings.data_dir.exists() {
            "ok"
        } else {
            "missing"
        }
    );
    println!("  database:    {}", settings.db_path.display());

    let storage = match SqliteStorage::connect_with_ai(&settings.db_path, settings.ai.enabled).await
    {
        Ok(storage) => {
            println!("  connect:     ok");
            storage
        }
        Err(e) => {
            println!("  connect:     FAILED — {e}");
            return Ok(());
        }
    };

    println!("\nAI");
    println!("  enabled:     {}", settings.ai.enabled);

    #[cfg(feature = "ai")]
    {
        use std::io::Write;

        println!(
            "  vector idx:  {}  ({})",
            if storage.ai_available() {
                "READY"
            } else {
                "unavailable"
            },
            storage.ai_status_reason()
        );
        let model = storage
            .get_metadata("embedding_model_id")
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "?".to_string());
        let dims = storage
            .get_metadata("embedding_dimensions")
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "?".to_string());
        println!("  embed model: {model} ({dims} dims)");
        {
            use crate::ai::Embedder;
            let embedder = crate::ai::active_embedder();
            let changed = storage
                .embedding_model_changed(embedder.id(), embedder.dimensions())
                .await
                .unwrap_or(false);
            if changed {
                println!(
                    "  active:      {} ({} dims) — index will rebuild on next launch",
                    embedder.id(),
                    embedder.dimensions()
                );
            }
        }
        let pending = storage.count_pending_embeddings().await.unwrap_or(0);
        println!("  to index:    {pending} note(s) pending");
        let (failed, last_error) = storage.failed_embedding_stats().await.unwrap_or((0, None));
        if failed > 0 {
            println!("  retrying:    {failed} note(s) after errors (exponential backoff)");
            if let Some(err) = last_error {
                println!("  last error:  {err}");
            }
        }

        let chat = &settings.ai.chat;
        let local = crate::ai::chat::is_local(&chat.base_url);
        println!("\nAsk (chat)");
        println!(
            "  endpoint:    {}  [{}]",
            chat.base_url,
            if local { "LOCAL" } else { "REMOTE" }
        );
        println!("  model:       {}", chat.model);
        println!("  allow remote: {}", chat.allow_remote);
        print!("  reachable:   ");
        let _ = std::io::stdout().flush();
        let reachable = crate::ai::chat::reachable(&chat.base_url).await;
        println!(
            "{}",
            if reachable {
                "yes"
            } else {
                "NO (is the LLM server running?)"
            }
        );
        if !local && !chat.allow_remote {
            println!("  note:        remote endpoint but allow_remote=false → Ask will refuse");
        }
    }

    #[cfg(not(feature = "ai"))]
    {
        let _ = &storage;
        println!("  (AI support is not compiled into this build)");
    }

    println!();
    Ok(())
}

/// Run the main TUI application loop.
async fn run_app(
    storage: SqliteStorage,
    postgres: Option<PostgresStorage>,
    settings: Settings,
    first_run: bool,
) -> Result<()> {
    let poll_interval = settings.sync.poll_interval_seconds;

    // Spawn persistence worker
    let (persist_tx, mut persist_rx) = workers::spawn_persistence_worker(storage.clone());

    // Spawn sync worker if PostgreSQL is available
    let (sync_tx, mut sync_rx) = spawn_sync_worker(postgres, storage.clone(), poll_interval);

    // Spawn background embedding worker (idles if AI/vector index unavailable)
    let mut embed_rx = workers::spawn_embedding_worker(storage.clone());

    // Spawn the ask-your-notes worker (inert without the ai feature)
    let (ask_tx, mut ask_rx) = workers::spawn_ask_worker(storage.clone(), settings.ai.chat.clone());

    // Setup terminal
    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    let backend = CrosstermBackend::new(&mut stdout);
    let mut terminal = Terminal::new(backend)?;

    // Handle panic: restore terminal state
    let result = run_tui_loop(
        &mut terminal,
        storage,
        persist_tx,
        &mut persist_rx,
        sync_tx,
        &mut sync_rx,
        &mut embed_rx,
        ask_tx,
        &mut ask_rx,
        settings,
        first_run,
    )
    .await;

    // Restore terminal regardless of outcome
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;

    if let Err(e) = result {
        tracing::error!("Application error: {:?}", e);
        eprintln!("Error: {}", e);
    }

    Ok(())
}

/// Optionally spawn the sync worker. Returns (sender, receiver) or defaults.
fn spawn_sync_worker(
    postgres: Option<PostgresStorage>,
    sqlite: SqliteStorage,
    poll_interval_seconds: u64,
) -> (Option<mpsc::Sender<SyncCommand>>, mpsc::Receiver<SyncEvent>) {
    match workers::spawn_sync_worker(postgres, sqlite, poll_interval_seconds) {
        Some((tx, rx)) => (Some(tx), rx),
        None => {
            // Create a dummy receiver that never delivers
            let (_, rx) = mpsc::channel(1);
            (None, rx)
        }
    }
}

/// The main TUI event loop.
#[allow(clippy::too_many_arguments)]
async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<&mut io::Stdout>>,
    storage: SqliteStorage,
    persist_tx: mpsc::Sender<PersistenceCommand>,
    persist_rx: &mut mpsc::Receiver<PersistenceEvent>,
    sync_tx: Option<mpsc::Sender<SyncCommand>>,
    sync_rx: &mut mpsc::Receiver<SyncEvent>,
    embed_rx: &mut mpsc::Receiver<workers::EmbeddingEvent>,
    ask_tx: mpsc::Sender<String>,
    ask_rx: &mut mpsc::Receiver<AskEvent>,
    settings: Settings,
    first_run: bool,
) -> Result<()> {
    // Enter alternate screen
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;

    // Start the event listener
    let mut events = events::start_event_listener();

    // Initialize app state
    let mut state = AppState::new();
    state.settings = settings.clone();
    state.preview_render_markdown = settings.preview.render_markdown;
    state.sync_enabled = sync_tx.is_some();
    state.sync_status = if sync_tx.is_some() {
        "ready".to_string()
    } else {
        "local".to_string()
    };
    state.set_status("Loading notes...");

    // Load initial notes
    match storage.list_notes().await {
        Ok(notes) => {
            let count = notes.len();
            state.set_notes(notes);
            state.set_status(format!("{} notes loaded", count));
        }
        Err(e) => {
            state.set_status(format!("Error loading notes: {}", e));
        }
    }

    // Seed the conflict indicator from any conflicts left over from a prior run.
    state.conflict_count = storage.count_unresolved_conflicts().await.unwrap_or(0);

    // Pull the latest immediately on startup (if sync is enabled) so a freshly
    // opened device shows current data without waiting for the first poll.
    if let Some(tx) = &sync_tx {
        let _ = tx.send(SyncCommand::SyncNow).await;
    }

    // On first run, greet the user with the setup screen.
    if first_run {
        state.open_settings(true);
        state.set_status(
            "Welcome to Jot! Set up storage & sync — Enter tests, Ctrl+S saves, Esc skips.",
        );
    }

    // Main loop
    loop {
        // Ensure the preview pane has the full body of the selected note.
        refresh_preview(&mut state, &storage).await;

        // Render
        terminal.draw(|frame| {
            layout::render(frame, &state);
        })?;

        // Process events — check both channels
        tokio::select! {
            Some(event) = events.recv() => {
                match handle_event(event, &mut state, &storage, &persist_tx, &sync_tx, &ask_tx).await {
                    Ok(true) => break, // true means quit
                    Ok(false) => {}    // continue
                    Err(e) => {
                        state.set_status(format!("Error: {}", e));
                    }
                }
            }
            Some(evt) = persist_rx.recv() => {
                handle_persistence_event(evt, &mut state, &storage).await;
            }
            Some(evt) = sync_rx.recv() => {
                handle_sync_event(evt, &mut state, &storage).await;
            }
            Some(evt) = embed_rx.recv() => {
                handle_embedding_event(evt, &mut state);
            }
            Some(evt) = ask_rx.recv() => {
                handle_ask_event(evt, &mut state);
            }
        }
    }

    // Cleanup
    crossterm::execute!(io::stdout(), crossterm::cursor::Show)?;

    Ok(())
}

/// Handle a single UI event, returning `true` if the app should quit.
///
/// Dispatch is view-first: each view interprets the low-level events (notably
/// `Char`) as either commands or literal text. This is what allows letters like
/// `j`, `n`, or `s` to be typed into the search box, titles, and settings form
/// while still acting as commands in the note list.
async fn handle_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
    sync_tx: &Option<mpsc::Sender<SyncCommand>>,
    ask_tx: &mpsc::Sender<String>,
) -> Result<bool> {
    match state.view {
        AppView::Editor => handle_editor_event(event, state, storage, persist_tx).await,
        AppView::EditorSearch => handle_editor_search_event(event, state).await,
        AppView::Settings => handle_settings_event(event, state, storage).await,
        AppView::ConfirmDelete => {
            handle_confirm_delete_event(event, state, storage, persist_tx).await
        }
        AppView::ConfirmReindex => handle_confirm_reindex_event(event, state, storage).await,
        AppView::Help => handle_help_event(event, state).await,
        AppView::CommandPalette => {
            handle_palette_event(event, state, storage, persist_tx, sync_tx).await
        }
        AppView::Trash => handle_trash_event(event, state, storage).await,
        AppView::ConfirmPurge => handle_confirm_purge_event(event, state, storage).await,
        AppView::ConfirmRunBlock => {
            handle_confirm_run_block_event(event, state, persist_tx).await
        }
        AppView::Search
        | AppView::Edit
        | AppView::Tag
        | AppView::TagRemove
        | AppView::TagFilter => handle_text_event(event, state, storage, persist_tx).await,
        AppView::ConflictReview | AppView::ConflictDetail => {
            handle_conflict_event(event, state, storage, persist_tx).await
        }
        AppView::Ask => handle_ask_view_event(event, state, storage, ask_tx).await,
        AppView::PreviewSearch => handle_preview_search_event(event, state).await,
        AppView::List | AppView::Preview => {
            handle_command_event(event, state, storage, persist_tx, sync_tx).await
        }
    }
}

/// Command-mode handling for the note list / preview. Printable characters are
/// interpreted as commands here.
async fn handle_command_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
    sync_tx: &Option<mpsc::Sender<SyncCommand>>,
) -> Result<bool> {
    if state.view == AppView::Preview {
        // The outline overlay captures navigation while it is open.
        if state.outline_open {
            match event {
                AppEvent::Up | AppEvent::Char('k') => state.outline_move(-1),
                AppEvent::Down | AppEvent::Char('j') => state.outline_move(1),
                AppEvent::Confirm => {
                    state.jump_to_selected_heading();
                    state.close_outline();
                }
                AppEvent::Cancel => state.close_outline(),
                _ => {}
            }
            return Ok(false);
        }
        match event {
            AppEvent::Up => state.scroll_preview_lines(-1),
            AppEvent::Down => state.scroll_preview_lines(1),
            AppEvent::PageUp => state.scroll_preview_pages(-1),
            AppEvent::PageDown => state.scroll_preview_pages(1),
            AppEvent::Cancel => {
                state.clear_code_focus();
                state.view = AppView::List;
                state.set_status("Preview closed");
            }
            AppEvent::Tab => focus_next_code_block(state),
            AppEvent::Confirm => open_focused_link(state, storage).await,
            AppEvent::Char(c) => match c {
                'q' => {
                    state.should_quit = true;
                    return Ok(true);
                }
                'j' => state.scroll_preview_lines(1),
                'k' => state.scroll_preview_lines(-1),
                'g' => state.reset_preview_scroll(),
                'G' => state.scroll_preview_to_bottom(),
                'o' => state.open_outline(),
                '/' => {
                    state.open_preview_search();
                    state.set_status("Find — type to search · Enter keep · Esc clear");
                }
                'n' => state.preview_search_step(1),
                'N' => state.preview_search_step(-1),
                'z' => state.toggle_fold_at_scroll(),
                'Z' => state.toggle_fold_all(),
                'w' => state.toggle_zen_mode(),
                '?' => open_help(state),
                ':' => state.open_palette(),
                ']' => focus_next_code_block(state),
                '[' => focus_prev_code_block(state),
                'y' => copy_focused_code_block(state),
                'x' => request_run_focused_block(state),
                d @ '1'..='9' => {
                    let index = d as usize - '1' as usize;
                    apply_suggested_tag(state, storage, persist_tx, index).await;
                }
                _ => {}
            },
            _ => {}
        }
        return Ok(false);
    }

    match event {
        AppEvent::Up => state.select_previous(),
        AppEvent::Down => state.select_next(),
        AppEvent::PageUp => state.scroll_preview_pages(-1),
        AppEvent::PageDown => state.scroll_preview_pages(1),
        AppEvent::Confirm => {
            if state.selected_note().is_some() {
                state.view = AppView::Preview;
                state.clear_code_focus();
                state.set_status(
                    "PREVIEW — j/k scroll · o outline · / find · z fold · w zen · ]/[ focus · Enter link · y copy · Esc back",
                );
            } else {
                state.set_status("No note selected");
            }
        }
        // Navigation stays inline; everything else dispatches through the
        // command registry so a key and its palette entry behave identically.
        AppEvent::Char(c) => match c {
            'j' => state.select_next(),
            'k' => state.select_previous(),
            'g' => state.reset_preview_scroll(),
            'G' => state.scroll_preview_to_bottom(),
            ':' => state.open_palette(),
            d @ '1'..='9' => {
                let index = d as usize - '1' as usize;
                apply_suggested_tag(state, storage, persist_tx, index).await;
            }
            other => {
                if let Some(id) = commands::command_for_key(other) {
                    return run_command(id, state, storage, persist_tx, sync_tx).await;
                }
            }
        },
        _ => {}
    }

    Ok(false)
}

/// Select `note_id` in the list and open the body editor on it. Clears any tag
/// filter first so the note is guaranteed to be in `state.notes` (the editor
/// saves to the *selected* note).
async fn open_note_in_editor(state: &mut AppState, storage: &SqliteStorage, note_id: Uuid) {
    state.tag_filter = None;
    reload_notes(state, storage).await;
    if let Some(i) = state.notes.iter().position(|n| n.id == note_id) {
        state.list_state.select(Some(i));
    }
    match storage.get_note(note_id).await {
        Ok(Some(full)) => {
            state.body_buffer = full.body;
            state.cursor_pos = state.body_buffer.len();
            state.wiki_complete = None;
            state.reset_edit_history();
            state.view = AppView::Editor;
            state.set_status("EDITOR — Ctrl+S save · Ctrl+P preview · Esc discard");
        }
        Ok(None) => state.set_status("Note no longer exists"),
        Err(e) => state.set_status(format!("Error: {}", e)),
    }
}

/// Execute a registry command. Returns `Ok(true)` to quit. Shared by the
/// keybindings and the command palette so both paths behave identically.
async fn run_command(
    id: CommandId,
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
    sync_tx: &Option<mpsc::Sender<SyncCommand>>,
) -> Result<bool> {
    match id {
        CommandId::NewNote => {
            let action = AppAction::CreateNote {
                title: "Untitled".to_string(),
            };
            match actions::handle_action(persist_tx, action).await {
                Ok(_) => {
                    reload_notes(state, storage).await;
                    // New note sorts to the top — select it and jump straight
                    // into title entry so the user can name it immediately.
                    if !state.notes.is_empty() {
                        state.list_state.select(Some(0));
                    }
                    state.edit_buffer.clear();
                    state.view = AppView::Edit;
                    state.set_status(
                        "New note — type a title, Enter to confirm (then 'i' to write the body)",
                    );
                }
                Err(e) => state.set_status(format!("Error: {}", e)),
            }
        }
        CommandId::RenameNote => {
            if state.selected_note().is_some() {
                state.edit_buffer.clear();
                state.view = AppView::Edit;
                state.set_status("Type new title, Enter to confirm. Esc to cancel.");
            } else {
                state.set_status("No note selected");
            }
        }
        CommandId::EditBody => {
            if let Some(note) = state.selected_note().cloned() {
                match storage.get_note(note.id).await {
                    Ok(Some(full_note)) => {
                        state.body_buffer = full_note.body;
                        state.cursor_pos = state.body_buffer.len();
                        state.reset_edit_history();
                        state.view = AppView::Editor;
                        state.set_status("EDITOR — Ctrl+S save · Ctrl+P preview · Esc discard");
                    }
                    Ok(None) => state.set_status("Note no longer exists"),
                    Err(e) => state.set_status(format!("Error: {}", e)),
                }
            } else {
                state.set_status("No note selected");
            }
        }
        CommandId::DailyNote => {
            let title = state.settings.notes.daily_title();
            let template = state.settings.notes.daily_template.clone();
            let daily_date = state.settings.notes.daily_date();
            let rollup = state.settings.notes.rollup_tasks;
            match storage
                .find_or_create_daily_note(&title, &template, &daily_date, rollup)
                .await
            {
                Ok((id, created)) => {
                    open_note_in_editor(state, storage, id).await;
                    state.set_status(if created {
                        format!("Created daily note — {title}")
                    } else {
                        format!("Daily note — {title}")
                    });
                }
                Err(e) => state.set_status(format!("Daily note failed: {e}")),
            }
        }
        CommandId::DeleteNote => {
            if let Some(note) = state.selected_note().cloned() {
                state.view = AppView::ConfirmDelete;
                state.set_status(format!(
                    "Delete \"{}\"? Press y to confirm, n or Esc to cancel.",
                    note.title
                ));
            } else {
                state.set_status("No note selected");
            }
        }
        CommandId::OpenTrash => open_trash(state, storage).await,
        CommandId::AddTag => {
            if state.selected_note().is_some() {
                state.edit_buffer.clear();
                state.view = AppView::Tag;
                state.set_status("Type tag name, Enter to add. Esc to cancel.");
            } else {
                state.set_status("No note selected");
            }
        }
        CommandId::RemoveTag => {
            if state.selected_note().is_some() {
                state.edit_buffer.clear();
                state.view = AppView::TagRemove;
                state.set_status("Type tag name, Enter to remove. Esc to cancel.");
            } else {
                state.set_status("No note selected");
            }
        }
        CommandId::FilterByTag => {
            state.edit_buffer.clear();
            state.view = AppView::TagFilter;
            state.set_status("Type tag name to filter by, Enter to confirm. Esc to cancel.");
        }
        CommandId::Search => {
            state.search_query.clear();
            state.search_kind = state.default_search_kind();
            state.view = AppView::Search;
            state.set_status(format!(
                "Search [{}] — Tab toggles keyword/semantic, Enter confirms, Esc cancels",
                state.search_kind.label()
            ));
        }
        CommandId::Ask => {
            state.open_ask();
            state.set_status("Ask your notes — type a question, Enter to ask, Esc to close");
        }
        CommandId::Reindex => {
            #[cfg(feature = "ai")]
            if storage.ai_available() {
                state.view = AppView::ConfirmReindex;
                state.set_status(
                    "Rebuild the embedding index? Every note is re-embedded. y = confirm, n = cancel",
                );
            } else {
                state.set_status("Semantic index unavailable — nothing to reindex");
            }
            #[cfg(not(feature = "ai"))]
            state.set_status("AI support is not compiled into this build");
        }
        CommandId::Export => export_notes(state, storage).await,
        CommandId::Import => import_notes(state, storage).await,
        CommandId::SyncNow => {
            if let Some(tx) = sync_tx {
                match tx.send(SyncCommand::SyncNow).await {
                    Ok(_) => state.set_status("Syncing..."),
                    Err(_) => state.set_status("Sync worker not available"),
                }
            } else {
                state.set_status("Sync not configured — press , to open Settings");
            }
        }
        CommandId::ReviewConflicts => match storage.list_conflicts().await {
            Ok(conflicts) => {
                state.conflicts = conflicts;
                state.conflict_index = 0;
                state.conflict_count = state.conflicts.len();
                if state.conflicts.is_empty() {
                    state.set_status("No unresolved conflicts");
                } else {
                    state.view = AppView::ConflictReview;
                    state.set_status(format!("{} unresolved conflict(s)", state.conflicts.len()));
                }
            }
            Err(e) => state.set_status(format!("Failed to load conflicts: {}", e)),
        },
        CommandId::Settings => {
            state.open_settings(false);
            state.set_status(
                "Settings — ↑/↓ move, type to edit, Enter tests, Ctrl+S saves, Esc closes",
            );
        }
        CommandId::Help => open_help(state),
        CommandId::Quit => {
            state.should_quit = true;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Handle events while in a single-line text-input view (search, title edit,
/// tag add/remove, tag filter).
async fn handle_text_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
) -> Result<bool> {
    match event {
        AppEvent::Char(c) => {
            if state.view == AppView::Search {
                state.search_query.push(c);
            } else {
                state.edit_buffer.push(c);
            }
        }
        AppEvent::Backspace => {
            if state.view == AppView::Search {
                state.search_query.pop();
            } else {
                state.edit_buffer.pop();
            }
        }
        AppEvent::Tab => {
            if state.view == AppView::Search {
                state.toggle_search_kind();
                state.set_status(format!(
                    "Search [{}] — Tab toggles keyword/semantic, Enter confirms",
                    state.search_kind.label()
                ));
            }
        }
        AppEvent::Confirm => match state.view {
            AppView::Search => {
                if !state.search_query.is_empty() {
                    let query = state.search_query.clone();
                    let kind = state.search_kind;
                    match run_search(storage, &query, kind).await {
                        Ok(notes) => {
                            let count = notes.len();
                            state.set_notes(notes);
                            state.view = AppView::List;
                            state.set_status(format!(
                                "{} search: \"{}\" — {} result(s)",
                                kind.label(),
                                query,
                                count
                            ));
                        }
                        Err(e) => state.set_status(format!("Search error: {}", e)),
                    }
                } else {
                    state.view = AppView::List;
                }
            }
            AppView::Edit => {
                if let Some(note) = state.selected_note().cloned() {
                    let new_title = if state.edit_buffer.is_empty() {
                        note.title.clone()
                    } else {
                        state.edit_buffer.clone()
                    };
                    match storage.get_note(note.id).await {
                        Ok(Some(full_note)) => {
                            let input = UpdateNoteInput {
                                id: note.id,
                                title: new_title,
                                body: full_note.body,
                            };
                            let (reply, rx) = oneshot::channel();
                            if persist_tx
                                .send(PersistenceCommand::UpdateNote { input, reply })
                                .await
                                .is_ok()
                            {
                                match rx.await {
                                    Ok(Ok(_)) => {
                                        state.set_status(
                                            "Title saved — press 'i' to write the note body",
                                        );
                                        state.edit_buffer.clear();
                                        state.view = AppView::List;
                                        reload_notes(state, storage).await;
                                    }
                                    Ok(Err(e)) => state.set_status(format!("Error: {}", e)),
                                    Err(_) => state.set_status("Worker channel closed"),
                                }
                            }
                        }
                        Ok(None) => {
                            state.set_status("Note no longer exists");
                            state.view = AppView::List;
                        }
                        Err(e) => state.set_status(format!("Error: {}", e)),
                    }
                }
            }
            AppView::Tag => {
                if let Some(note) = state.selected_note().cloned() {
                    let tag_name = state.edit_buffer.clone();
                    if !tag_name.is_empty() {
                        let (reply, rx) = oneshot::channel();
                        if persist_tx
                            .send(PersistenceCommand::AddTag {
                                note_id: note.id,
                                tag_name,
                                reply,
                            })
                            .await
                            .is_ok()
                        {
                            match rx.await {
                                Ok(Ok(_)) => {
                                    state.set_status("Tag added");
                                    state.edit_buffer.clear();
                                    state.view = AppView::List;
                                    reload_notes(state, storage).await;
                                }
                                Ok(Err(e)) => state.set_status(format!("Error: {}", e)),
                                Err(_) => state.set_status("Worker channel closed"),
                            }
                        }
                    } else {
                        state.view = AppView::List;
                    }
                }
            }
            AppView::TagRemove => {
                if let Some(note) = state.selected_note().cloned() {
                    let tag_name = state.edit_buffer.clone();
                    if !tag_name.is_empty() {
                        let (reply, rx) = oneshot::channel();
                        if persist_tx
                            .send(PersistenceCommand::RemoveTag {
                                note_id: note.id,
                                tag_name,
                                reply,
                            })
                            .await
                            .is_ok()
                        {
                            match rx.await {
                                Ok(Ok(_)) => {
                                    state.set_status("Tag removed");
                                    state.edit_buffer.clear();
                                    state.view = AppView::List;
                                    reload_notes(state, storage).await;
                                }
                                Ok(Err(e)) => state.set_status(format!("Error: {}", e)),
                                Err(_) => state.set_status("Worker channel closed"),
                            }
                        }
                    } else {
                        state.view = AppView::List;
                    }
                }
            }
            AppView::TagFilter => {
                let tag_name = state.edit_buffer.clone();
                if tag_name.is_empty() {
                    state.tag_filter = None;
                    state.view = AppView::List;
                    reload_notes(state, storage).await;
                    state.set_status("Tag filter cleared");
                } else {
                    match storage.list_notes_by_tag(&tag_name).await {
                        Ok(notes) => {
                            state.tag_filter = Some(tag_name.clone());
                            state.set_notes(notes);
                            state.edit_buffer.clear();
                            state.view = AppView::List;
                            state.set_status(format!("Filtered by tag: {}", tag_name));
                        }
                        Err(e) => state.set_status(format!("Error: {}", e)),
                    }
                }
            }
            _ => state.view = AppView::List,
        },
        AppEvent::Cancel => match state.view {
            AppView::Search => {
                state.search_query.clear();
                state.view = AppView::List;
                reload_notes(state, storage).await;
                state.set_status("Cancelled");
            }
            AppView::TagFilter => {
                state.edit_buffer.clear();
                state.tag_filter = None;
                state.view = AppView::List;
                reload_notes(state, storage).await;
                state.set_status("Filter cleared");
            }
            _ => {
                state.edit_buffer.clear();
                state.view = AppView::List;
                state.set_status("Cancelled");
            }
        },
        // Direction keys are no-ops in single-line inputs.
        _ => {}
    }

    Ok(false)
}

/// Handle events in the conflict review list and conflict detail views.
async fn handle_conflict_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
) -> Result<bool> {
    match state.view {
        AppView::ConflictReview => match event {
            AppEvent::Up => state.select_previous_conflict(),
            AppEvent::Down => state.select_next_conflict(),
            AppEvent::Confirm => {
                if state.selected_conflict().is_some() {
                    state.view = AppView::ConflictDetail;
                    state.set_status("1: keep local | 2: keep remote | 3: save both");
                }
            }
            AppEvent::Cancel => {
                state.view = AppView::List;
                state.set_status("Conflict resolution cancelled");
            }
            AppEvent::Char(c) => match c {
                'q' => {
                    state.should_quit = true;
                    return Ok(true);
                }
                'j' => state.select_next_conflict(),
                'k' => state.select_previous_conflict(),
                _ => {}
            },
            _ => {}
        },
        AppView::ConflictDetail => match event {
            AppEvent::Cancel => {
                state.view = AppView::List;
                state.set_status("Conflict resolution cancelled");
            }
            AppEvent::Char(c) => match c {
                '1' => {
                    return resolve_conflict_action(
                        state,
                        storage,
                        persist_tx,
                        ConflictResolution::KeepLocal,
                    )
                    .await;
                }
                '2' => {
                    return resolve_conflict_action(
                        state,
                        storage,
                        persist_tx,
                        ConflictResolution::KeepRemote,
                    )
                    .await;
                }
                '3' => {
                    return resolve_conflict_action(
                        state,
                        storage,
                        persist_tx,
                        ConflictResolution::SaveBoth,
                    )
                    .await;
                }
                'q' => {
                    state.should_quit = true;
                    return Ok(true);
                }
                _ => {}
            },
            _ => {}
        },
        _ => {}
    }

    Ok(false)
}

/// Handle events for the settings / first-run setup screen.
async fn handle_settings_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
) -> Result<bool> {
    match event {
        AppEvent::Up => state.settings_form.prev_field(),
        AppEvent::Down => state.settings_form.next_field(),
        AppEvent::Left | AppEvent::Right => state.settings_form.toggle_or_cycle(),
        AppEvent::Char(c) => {
            if state.settings_form.on_toggle_field() {
                if c == ' ' {
                    state.settings_form.toggle_or_cycle();
                }
            } else {
                state.settings_form.input_char(c);
            }
        }
        AppEvent::Backspace => state.settings_form.backspace(),
        AppEvent::Confirm => test_connection(state, storage).await,
        AppEvent::Save => {
            let mut new_settings = state.settings.clone();
            match state.settings_form.apply_to(&mut new_settings) {
                Ok(()) => match new_settings.save() {
                    Ok(()) => {
                        let first = state.settings_form.first_run;
                        state.settings = new_settings;
                        state.view = AppView::List;
                        if first {
                            state.set_status("Setup saved to config.toml. Welcome to Jot!");
                        } else {
                            state.set_status(
                                "Settings saved (restart to apply storage/sync changes)",
                            );
                        }
                    }
                    Err(e) => state.settings_form.status = format!("Save failed: {}", e),
                },
                Err(e) => state.settings_form.status = e,
            }
        }
        AppEvent::Cancel => {
            if state.settings_form.first_run {
                // Persist on first-run skip so the welcome doesn't reappear.
                let mut new_settings = state.settings.clone();
                if state.settings_form.apply_to(&mut new_settings).is_err() {
                    new_settings = state.settings.clone();
                }
                let _ = new_settings.save();
                state.settings = new_settings;
                state.set_status("Setup skipped — defaults saved. Press , to configure later.");
            } else {
                state.set_status("Settings closed");
            }
            state.view = AppView::List;
        }
        _ => {}
    }

    Ok(false)
}

/// Handle the y/n delete confirmation prompt.
async fn handle_confirm_delete_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
) -> Result<bool> {
    match event {
        AppEvent::Char('y') | AppEvent::Char('Y') => {
            if let Some(note) = state.selected_note().cloned() {
                let action = AppAction::DeleteNote { note_id: note.id };
                match actions::handle_action(persist_tx, action).await {
                    Ok(_) => {
                        state.set_status(format!("Deleted \"{}\"", note.title));
                        reload_notes(state, storage).await;
                    }
                    Err(e) => state.set_status(format!("Error: {}", e)),
                }
            }
            state.view = AppView::List;
        }
        AppEvent::Char('n') | AppEvent::Char('N') | AppEvent::Cancel => {
            state.view = AppView::List;
            state.set_status("Delete cancelled");
        }
        _ => {}
    }
    Ok(false)
}

/// Confirm (y/n) a full rebuild of the embedding index. On confirm, every live
/// note is re-enqueued for the active model and the background worker re-embeds
/// it; the status-bar index indicator reflects the new backlog immediately.
async fn handle_confirm_reindex_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
) -> Result<bool> {
    match event {
        AppEvent::Char('y') | AppEvent::Char('Y') => {
            #[cfg(feature = "ai")]
            {
                use crate::ai::Embedder;
                let embedder = crate::ai::active_embedder();
                match storage
                    .reindex_embeddings(embedder.id(), embedder.dimensions())
                    .await
                {
                    Ok(n) => {
                        state.embed_pending = n as usize;
                        state.set_status(format!(
                            "Reindexing {n} note(s) — re-embedding in the background"
                        ));
                    }
                    Err(e) => state.set_status(format!("Reindex failed: {e}")),
                }
            }
            #[cfg(not(feature = "ai"))]
            {
                let _ = storage;
                state.set_status("AI support is not compiled into this build");
            }
            state.view = AppView::List;
        }
        AppEvent::Char('n') | AppEvent::Char('N') | AppEvent::Cancel => {
            state.view = AppView::List;
            state.set_status("Reindex cancelled");
        }
        _ => {}
    }
    Ok(false)
}

/// Focus the next fenced code block in the preview (code-block actions).
fn focus_next_code_block(state: &mut AppState) {
    if !preview_has_focusables(state) {
        return;
    }
    state.focus_next_code_block();
    announce_focused_block(state);
}

/// Focus the previous focusable (code block or link) in the preview.
fn focus_prev_code_block(state: &mut AppState) {
    if !preview_has_focusables(state) {
        return;
    }
    state.focus_prev_code_block();
    announce_focused_block(state);
}

/// Whether the preview currently has anything to focus, honoring the feature
/// gates: code blocks count only with `code_actions`, links only with
/// `wikilinks`. Sets an explanatory status when there's nothing.
fn preview_has_focusables(state: &mut AppState) -> bool {
    let has_code = state.settings.preview.code_actions && !state.preview_code_blocks.is_empty();
    let has_links = !state.preview_links.is_empty()
        || !state.backlinks.is_empty()
        || !state.on_this_day.is_empty();
    if !has_code && !has_links {
        state.set_status("Nothing to focus in this note");
        return false;
    }
    true
}

/// Status line describing the focused item and the actions available on it.
fn announce_focused_block(state: &mut AppState) {
    if let Some(idx) = state.focused_code_index() {
        let total = state.preview_code_blocks.len();
        let runnable = state
            .focused_code_block()
            .is_some_and(|b| b.is_runnable() && state.settings.preview.allow_run);
        let run_hint = if runnable { " · x run" } else { "" };
        state.set_status(format!(
            "Code block {}/{} — y copy{} · ]/[ next/prev · Esc clear",
            idx + 1,
            total,
            run_hint
        ));
    } else if let Some((display, target)) = state.focused_nav_target() {
        let action = if target.is_some() {
            "Enter opens"
        } else {
            "dangling (no such note)"
        };
        state.set_status(format!(
            "→ {display} — {action} · ]/[ next/prev · Esc clear"
        ));
    }
}

/// Copy the focused code block's source to the clipboard via OSC 52.
fn copy_focused_code_block(state: &mut AppState) {
    if !state.settings.preview.code_actions {
        return;
    }
    let Some(block) = state.focused_code_block() else {
        state.set_status("No code block focused — press ] to focus one");
        return;
    };
    let code = block.code.clone();
    match tui::clipboard::copy_to_clipboard(&code) {
        Ok(()) => state.set_status(format!("Copied {} line(s) to clipboard", code.lines().count())),
        Err(e) => state.set_status(format!("Clipboard copy failed: {e}")),
    }
}

/// Begin the run flow: validate the focused block is runnable, then ask for
/// confirmation before executing anything.
fn request_run_focused_block(state: &mut AppState) {
    if !state.settings.preview.code_actions {
        return;
    }
    if !state.settings.preview.allow_run {
        state.set_status("Running code blocks is disabled ([preview] allow_run = false)");
        return;
    }
    let Some(block) = state.focused_code_block() else {
        state.set_status("No code block focused — press ] to focus one");
        return;
    };
    if !block.is_runnable() {
        let lang = block.lang.clone().unwrap_or_default();
        state.set_status(format!("Can't run {lang} blocks — only shell blocks are runnable"));
        return;
    }
    let label = block.label();
    state.view = AppView::ConfirmRunBlock;
    state.set_status(format!("Run {label}? Output is captured into the note. y = run, n = cancel"));
}

/// Confirm (y/n) executing the focused code block. On confirm, run it, splice
/// the captured output into the note body, persist, and refresh the preview.
async fn handle_confirm_run_block_event(
    event: AppEvent,
    state: &mut AppState,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
) -> Result<bool> {
    match event {
        AppEvent::Char('y') | AppEvent::Char('Y') => {
            run_focused_block(state, persist_tx).await;
            state.view = AppView::Preview;
        }
        AppEvent::Char('n') | AppEvent::Char('N') | AppEvent::Cancel => {
            state.view = AppView::Preview;
            state.set_status("Run cancelled");
        }
        _ => {}
    }
    Ok(false)
}

/// Maximum bytes of captured run output written back into a note.
const MAX_RUN_OUTPUT_BYTES: usize = 64 * 1024;
/// Wall-clock timeout for a run.
const RUN_TIMEOUT_SECS: u64 = 30;

/// Execute the focused code block via the user's shell, capture combined
/// stdout+stderr (timed and size-capped), splice it into the note body as an
/// ` ```output ` block, persist the note, and refresh the preview.
async fn run_focused_block(
    state: &mut AppState,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
) {
    let Some(block) = state.focused_code_block().cloned() else {
        state.set_status("No code block focused");
        return;
    };
    let Some(note) = state.selected_note().cloned() else {
        state.set_status("No note selected");
        return;
    };

    state.set_status("Running…");
    let output = match execute_shell(&block.code, &state.settings.data_dir).await {
        Ok(out) => out,
        Err(e) => {
            state.set_status(format!("Run failed: {e}"));
            return;
        }
    };

    // Splice the captured output after the focused block, then persist.
    let new_body =
        markdown::insert_or_replace_output(&state.preview_body, block.end_byte, &output);
    let input = UpdateNoteInput {
        id: note.id,
        title: note.title.clone(),
        body: new_body.clone(),
    };
    let (reply, rx) = oneshot::channel();
    if persist_tx
        .send(PersistenceCommand::UpdateNote { input, reply })
        .await
        .is_err()
    {
        state.set_status("Worker channel closed");
        return;
    }
    match rx.await {
        Ok(Ok(_)) => {
            state.preview_body = new_body;
            state.refresh_code_blocks();
            // Keep the same block focused if it still exists.
            state.preview_note_id = Some(note.id);
            state.set_status("Ran block — output captured into the note");
        }
        Ok(Err(e)) => state.set_status(format!("Save failed: {e}")),
        Err(_) => state.set_status("Worker channel closed"),
    }
}

/// Run `code` through the user's `$SHELL` (falling back to `sh`) in `cwd`,
/// returning combined stdout+stderr. Times out after `RUN_TIMEOUT_SECS` and
/// caps the captured output at `MAX_RUN_OUTPUT_BYTES`.
async fn execute_shell(code: &str, cwd: &std::path::Path) -> Result<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    let mut command = tokio::process::Command::new(&shell);
    command
        .arg("-c")
        .arg(code)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null());

    let run = command.output();
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(RUN_TIMEOUT_SECS),
        run,
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Ok(format!(
                "[timed out after {RUN_TIMEOUT_SECS}s — process killed]"
            ));
        }
    };

    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    if !output.status.success() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&format!("[exit status: {}]", output.status));
    }
    if combined.trim().is_empty() {
        combined = "[no output]".to_string();
    }

    // Cap size, keeping a whole-char boundary.
    if combined.len() > MAX_RUN_OUTPUT_BYTES {
        let mut cut = MAX_RUN_OUTPUT_BYTES;
        while !combined.is_char_boundary(cut) {
            cut -= 1;
        }
        combined.truncate(cut);
        combined.push_str("\n[output truncated]");
    }

    Ok(combined)
}

/// Open the scrollable keybinding help overlay.
fn open_help(state: &mut AppState) {
    state.reset_preview_scroll();
    state.view = AppView::Help;
    state.set_status("Help — j/k scroll, Esc or ? to close");
}

/// Handle events in the help overlay: scroll, or close with Esc/?/q.
async fn handle_help_event(event: AppEvent, state: &mut AppState) -> Result<bool> {
    match event {
        AppEvent::Up => state.scroll_preview_lines(-1),
        AppEvent::Down => state.scroll_preview_lines(1),
        AppEvent::PageUp => state.scroll_preview_pages(-1),
        AppEvent::PageDown => state.scroll_preview_pages(1),
        AppEvent::Cancel => close_help(state),
        AppEvent::Char(c) => match c {
            'j' => state.scroll_preview_lines(1),
            'k' => state.scroll_preview_lines(-1),
            'g' => state.reset_preview_scroll(),
            'G' => state.scroll_preview_to_bottom(),
            'q' | '?' => close_help(state),
            _ => {}
        },
        _ => {}
    }
    Ok(false)
}

fn close_help(state: &mut AppState) {
    state.view = AppView::List;
    state.set_status("Help closed");
}

/// Handle events in the command palette: type to filter, ↑/↓ to move, Enter to
/// run the selected command, Esc to close.
async fn handle_palette_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
    sync_tx: &Option<mpsc::Sender<SyncCommand>>,
) -> Result<bool> {
    match event {
        AppEvent::Cancel => {
            state.view = AppView::List;
            state.set_status("Palette closed");
        }
        AppEvent::Up => state.palette_select_previous(),
        AppEvent::Down => state.palette_select_next(),
        AppEvent::Backspace => state.palette_backspace(),
        AppEvent::Confirm => {
            // Close first; the command may set its own destination view.
            let selected = state.selected_palette_command();
            state.view = AppView::List;
            match selected {
                Some(id) => return run_command(id, state, storage, persist_tx, sync_tx).await,
                None => state.set_status("No matching command"),
            }
        }
        AppEvent::Char(c) => state.palette_input(c),
        _ => {}
    }
    Ok(false)
}

/// Open the trash view, loading soft-deleted notes.
async fn open_trash(state: &mut AppState, storage: &SqliteStorage) {
    match storage.list_deleted_notes().await {
        Ok(notes) => {
            let n = notes.len();
            state.set_trash_notes(notes);
            state.view = AppView::Trash;
            if n == 0 {
                state.set_status("Trash is empty — Esc to go back");
            } else {
                state.set_status(format!(
                    "Trash — {n} note(s) · r restore · x purge · Esc back"
                ));
            }
        }
        Err(e) => state.set_status(format!("Failed to open trash: {e}")),
    }
}

/// Handle events while browsing the trash: navigate, restore, or purge.
async fn handle_trash_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
) -> Result<bool> {
    match event {
        AppEvent::Up => state.trash_select_previous(),
        AppEvent::Down => state.trash_select_next(),
        AppEvent::Cancel => {
            state.view = AppView::List;
            // A restore may have changed the live list — refresh it.
            reload_notes(state, storage).await;
            state.set_status("Trash closed");
        }
        AppEvent::Char(c) => match c {
            'j' => state.trash_select_next(),
            'k' => state.trash_select_previous(),
            'r' => {
                if let Some(note) = state.selected_trash_note().cloned() {
                    match storage.restore_note(note.id).await {
                        Ok(()) => {
                            state.set_status(format!("Restored \"{}\"", note.title));
                            if let Ok(notes) = storage.list_deleted_notes().await {
                                state.set_trash_notes(notes);
                            }
                        }
                        Err(e) => state.set_status(format!("Restore failed: {e}")),
                    }
                } else {
                    state.set_status("Trash is empty");
                }
            }
            'x' => {
                if let Some(note) = state.selected_trash_note().cloned() {
                    state.view = AppView::ConfirmPurge;
                    state.set_status(format!(
                        "Permanently delete \"{}\"? This cannot be undone. y / n",
                        note.title
                    ));
                } else {
                    state.set_status("Trash is empty");
                }
            }
            'q' => {
                state.should_quit = true;
                return Ok(true);
            }
            _ => {}
        },
        _ => {}
    }
    Ok(false)
}

/// Confirm (y/n) a permanent purge of the selected trashed note.
async fn handle_confirm_purge_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
) -> Result<bool> {
    match event {
        AppEvent::Char('y') | AppEvent::Char('Y') => {
            if let Some(note) = state.selected_trash_note().cloned() {
                match storage.purge_note(note.id).await {
                    Ok(()) => {
                        state.set_status(format!("Permanently deleted \"{}\"", note.title));
                        if let Ok(notes) = storage.list_deleted_notes().await {
                            state.set_trash_notes(notes);
                        }
                    }
                    Err(e) => state.set_status(format!("Purge failed: {e}")),
                }
            }
            state.view = AppView::Trash;
        }
        AppEvent::Char('n') | AppEvent::Char('N') | AppEvent::Cancel => {
            state.view = AppView::Trash;
            state.set_status("Purge cancelled");
        }
        _ => {}
    }
    Ok(false)
}

/// Export every note to a timestamped Markdown folder under the data dir.
async fn export_notes(state: &mut AppState, storage: &SqliteStorage) {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let dir = state
        .settings
        .data_dir
        .join("exports")
        .join(format!("jot-down-export-{stamp}"));
    match notes_io::export_notes(storage, &dir).await {
        Ok(summary) => state.set_status(format!(
            "Exported {} note(s) to {}",
            summary.written,
            summary.dir.display()
        )),
        Err(e) => state.set_status(format!("Export failed: {e}")),
    }
}

/// Import Markdown notes from the `import/` folder under the data dir.
async fn import_notes(state: &mut AppState, storage: &SqliteStorage) {
    let dir = state.settings.data_dir.join("import");
    match notes_io::import_notes(storage, &dir).await {
        Ok(summary) => {
            if summary.imported == 0 && summary.skipped == 0 {
                state.set_status(format!(
                    "No .md files in {} — drop files there and press I",
                    summary.dir.display()
                ));
            } else {
                state.set_status(format!(
                    "Imported {} note(s), skipped {} already present",
                    summary.imported, summary.skipped
                ));
                reload_notes(state, storage).await;
            }
        }
        Err(e) => state.set_status(format!("Import failed: {e}")),
    }
}

/// Test the Postgres connection currently entered in the settings form.
async fn test_connection(state: &mut AppState, storage: &SqliteStorage) {
    let url = state.settings_form.database_url.trim().to_string();
    if url.is_empty() {
        state.settings_form.status = "Enter a Postgres URL, then press Enter to test".to_string();
        return;
    }
    state.settings_form.status = "Testing connection…".to_string();
    let device_id = storage.get_or_create_device_id().await.unwrap_or_default();
    match PostgresStorage::connect(&url, device_id).await {
        Ok(_) => state.settings_form.status = "✓ Connection succeeded".to_string(),
        Err(e) => state.settings_form.status = format!("✗ {}", e),
    }
}

/// Handle events when in full editor (body editing) mode.
async fn handle_editor_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
) -> Result<bool> {
    // When the [[ autocomplete popup is open, these keys drive it instead of
    // editing the buffer.
    if state.wiki_complete.is_some() {
        match event {
            AppEvent::Up => {
                state.wiki_complete_move(-1);
                return Ok(false);
            }
            AppEvent::Down => {
                state.wiki_complete_move(1);
                return Ok(false);
            }
            AppEvent::Confirm | AppEvent::Tab => {
                if let Some(title) = state.wiki_complete_accept() {
                    state.set_status(format!("Inserted [[{title}]]"));
                }
                return Ok(false);
            }
            AppEvent::Cancel => {
                state.wiki_complete = None;
                state.set_status("Autocomplete dismissed");
                return Ok(false);
            }
            _ => {}
        }
    }

    match event {
        AppEvent::Char(c) => {
            state.begin_edit(EditKind::Insert);
            state.insert_at_cursor(c);
        }
        AppEvent::Confirm => {
            // Enter inserts a newline
            state.begin_edit(EditKind::Insert);
            state.insert_newline();
        }
        AppEvent::Backspace => {
            state.begin_edit(EditKind::Delete);
            state.backspace_at_cursor();
        }
        AppEvent::Delete => {
            state.begin_edit(EditKind::Delete);
            state.delete_at_cursor();
        }
        AppEvent::WordBackspace => {
            state.begin_edit(EditKind::Delete);
            state.delete_word_before_cursor();
        }
        AppEvent::Undo => {
            if !state.undo() {
                state.set_status("Nothing to undo");
            }
        }
        AppEvent::Redo => {
            if !state.redo() {
                state.set_status("Nothing to redo");
            }
        }
        AppEvent::Find => {
            state.open_editor_search();
            state.set_status("Find — type to search, Enter/↓ next, ↑ prev, Esc done");
        }
        AppEvent::TogglePreview => {
            state.editor_preview_split = !state.editor_preview_split;
            if state.editor_preview_split {
                state.editor_preview_scroll.set(0);
                state.set_status("Live preview on — Ctrl+P to hide");
            } else {
                state.set_status("Live preview off — Ctrl+P to show");
            }
        }
        AppEvent::Home => {
            state.break_edit_group();
            state.cursor_line_start();
        }
        AppEvent::End => {
            state.break_edit_group();
            state.cursor_line_end();
        }
        AppEvent::Left => {
            state.break_edit_group();
            state.cursor_left();
        }
        AppEvent::Right => {
            state.break_edit_group();
            state.cursor_right();
        }
        AppEvent::WordLeft => {
            state.break_edit_group();
            state.cursor_word_left();
        }
        AppEvent::WordRight => {
            state.break_edit_group();
            state.cursor_word_right();
        }
        AppEvent::Up => {
            state.break_edit_group();
            state.cursor_up();
        }
        AppEvent::Down => {
            state.break_edit_group();
            state.cursor_down();
        }
        AppEvent::Save => {
            // Ctrl+S: save body and return to list
            if let Some(note) = state.selected_note().cloned() {
                let input = UpdateNoteInput {
                    id: note.id,
                    title: note.title.clone(),
                    body: state.body_buffer.clone(),
                };
                let (reply, rx) = oneshot::channel();
                if persist_tx
                    .send(PersistenceCommand::UpdateNote { input, reply })
                    .await
                    .is_ok()
                {
                    match rx.await {
                        Ok(Ok(_)) => {
                            state.set_status("Note saved");
                            state.wiki_complete = None;
                            state.view = AppView::List;
                            reload_notes(state, storage).await;
                        }
                        Ok(Err(e)) => state.set_status(format!("Save error: {}", e)),
                        Err(_) => state.set_status("Worker channel closed"),
                    }
                }
            }
        }
        AppEvent::Cancel => {
            // Esc: discard changes and return to list
            state.body_buffer.clear();
            state.cursor_pos = 0;
            state.wiki_complete = None;
            state.reset_edit_history();
            state.view = AppView::List;
            state.set_status("Editing cancelled");
            reload_notes(state, storage).await;
        }
        _ => {}
    }

    // Recompute the [[ autocomplete after any buffer/cursor change.
    if state.view == AppView::Editor {
        let enabled = state.settings.preview.wikilinks;
        state.refresh_wiki_complete(enabled);
    }

    Ok(false)
}

/// Handle events while typing a find-in-preview query. Matches are recomputed
/// against the rendered preview each frame; Up/Down (or n/N after confirming)
/// step through them.
async fn handle_preview_search_event(event: AppEvent, state: &mut AppState) -> Result<bool> {
    match event {
        AppEvent::Char(c) => state.preview_search_input(c),
        AppEvent::Backspace => state.preview_search_backspace(),
        AppEvent::Confirm => {
            state.confirm_preview_search();
            state.set_status("Find — n next · N prev · / again · Esc clear");
        }
        AppEvent::Cancel => {
            state.cancel_preview_search();
            state.set_status("Find cleared");
        }
        AppEvent::Down => state.preview_search_step(1),
        AppEvent::Up => state.preview_search_step(-1),
        _ => {}
    }
    Ok(false)
}

/// Handle events in the in-note search (Find) view.
async fn handle_editor_search_event(event: AppEvent, state: &mut AppState) -> Result<bool> {
    match event {
        AppEvent::Char(c) => {
            state.editor_search_query.push(c);
            state.editor_search_refresh();
        }
        AppEvent::Backspace => {
            state.editor_search_query.pop();
            state.editor_search_refresh();
        }
        AppEvent::Confirm | AppEvent::Down => {
            state.editor_search_next();
        }
        AppEvent::Up => {
            state.editor_search_prev();
        }
        AppEvent::Cancel => {
            // Return to editor, cursor stays at the match position
            state.view = AppView::Editor;
            state.set_status("EDITOR — Ctrl+S save · Ctrl+P preview · Esc discard");
        }
        _ => {}
    }
    Ok(false)
}

/// Handle an event from the persistence worker.
async fn handle_persistence_event(
    event: PersistenceEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
) {
    match event {
        PersistenceEvent::NoteDeleted { note_id: _ } => {
            state.set_status("Note deleted");
            reload_notes(state, storage).await;
        }
        PersistenceEvent::Error { message } => {
            state.set_status(format!("Persistence error: {}", message));
        }
    }
}

/// Handle an event from the sync worker.
async fn handle_sync_event(event: SyncEvent, state: &mut AppState, storage: &SqliteStorage) {
    match event {
        SyncEvent::Started => {
            state.sync_status = "syncing".to_string();
            state.set_status("Syncing...");
        }
        SyncEvent::Completed { message } => {
            state.sync_status = "synced".to_string();
            state.set_status(format!("Sync: {}", message));
            // Refresh the visible list so pulled changes appear immediately —
            // but only while browsing, so we don't disturb an active edit (which
            // reloads on its own when it returns to the list).
            if matches!(state.view, AppView::List | AppView::Preview) {
                reload_notes(state, storage).await;
            }
            // Keep the conflict indicator current regardless of view.
            if let Ok(count) = storage.count_unresolved_conflicts().await {
                state.conflict_count = count;
            }
        }
        SyncEvent::Failed { message } => {
            state.sync_status = "error".to_string();
            state.set_status(format!("Sync error: {}", message));
        }
    }
}

/// Handle an event from the background embedding worker.
fn handle_embedding_event(event: workers::EmbeddingEvent, state: &mut AppState) {
    match event {
        workers::EmbeddingEvent::Progress { embedded, pending } => {
            state.embed_pending = pending.max(0) as usize;
            if embedded > 0 {
                let suffix = if pending > 0 {
                    format!(", {pending} pending")
                } else {
                    String::new()
                };
                state.set_status(format!("Indexed {embedded} note(s){suffix}"));
            }
        }
        workers::EmbeddingEvent::Unavailable { reason } => {
            tracing::info!("Embedding worker idle: {}", reason);
        }
    }
}

/// Handle an answer (or error) from the Ask worker.
fn handle_ask_event(event: AskEvent, state: &mut AppState) {
    match event {
        AskEvent::Answer { text, citations } => {
            state.ask_pending = false;
            state.ask_answer = Some(text);
            state.ask_citations = citations;
            state.ask_input.clear();
            let n = state.ask_citations.len();
            state.set_status(format!(
                "Answer ready — {n} source(s); press a number to open one, Esc to close"
            ));
        }
        AskEvent::Error(message) => {
            state.ask_pending = false;
            state.set_status(format!("Ask error: {message}"));
        }
    }
}

/// Handle events while in the Ask view: type a question, submit it, or open a
/// cited note by its number.
async fn handle_ask_view_event(
    event: AppEvent,
    state: &mut AppState,
    storage: &SqliteStorage,
    ask_tx: &mpsc::Sender<String>,
) -> Result<bool> {
    match event {
        AppEvent::Cancel => {
            state.view = AppView::List;
            state.set_status("Ask closed");
        }
        AppEvent::Confirm => {
            let question = state.ask_input.trim().to_string();
            if !question.is_empty() && !state.ask_pending {
                match ask_tx.send(question).await {
                    Ok(_) => {
                        state.ask_pending = true;
                        state.ask_answer = None;
                        state.ask_citations.clear();
                        state.set_status("Thinking…");
                    }
                    Err(_) => {
                        state.set_status("Ask worker unavailable (is the `ai` feature enabled?)")
                    }
                }
            }
        }
        AppEvent::Backspace => {
            state.ask_input.pop();
        }
        AppEvent::Char(c) => {
            // Once an answer is shown and the input is empty, digits open the
            // cited note rather than starting a new question.
            if state.ask_answer.is_some() && state.ask_input.is_empty() && c.is_ascii_digit() {
                if let Some(n) = c.to_digit(10) {
                    open_citation(state, storage, n as usize).await;
                    return Ok(false);
                }
            }
            state.ask_input.push(c);
        }
        _ => {}
    }
    Ok(false)
}

/// Open the note behind citation number `n` (if present) in the preview.
async fn open_citation(state: &mut AppState, storage: &SqliteStorage, n: usize) {
    let Some(citation) = state.ask_citations.iter().find(|c| c.index == n).cloned() else {
        return;
    };
    reload_notes(state, storage).await;
    if let Some(pos) = state
        .notes
        .iter()
        .position(|note| note.id == citation.note_id)
    {
        state.list_state.select(Some(pos));
        state.view = AppView::Preview;
        state.set_status(format!("Opened [{}] {}", citation.index, citation.title));
    } else {
        state.set_status("Cited note not found (it may be filtered or deleted)");
    }
}

/// Open the currently focused `[[wikilink]]` in the preview, jumping to its
/// target note. No-op (with a status) when nothing's focused or the link is
/// dangling. Mirrors `open_citation`: select the note and stay in Preview.
async fn open_focused_link(state: &mut AppState, storage: &SqliteStorage) {
    let Some((display, target)) = state.focused_nav_target() else {
        return;
    };
    let Some(target) = target else {
        state.set_status(format!("\"{display}\" — no such note (dangling link)"));
        return;
    };

    reload_notes(state, storage).await;
    if let Some(pos) = state.notes.iter().position(|n| n.id == target) {
        state.list_state.select(Some(pos));
        state.clear_code_focus();
        state.set_status(format!("Opened {display}"));
    } else {
        state.set_status("Linked note not found (it may be filtered or deleted)");
    }
}

/// Load the full body of the selected note into the preview, if the selection
/// changed since the last load. Cheap when the selection is unchanged.
async fn refresh_preview(state: &mut AppState, storage: &SqliteStorage) {
    let selected_id = state.selected_note().map(|n| n.id);
    if selected_id == state.preview_note_id {
        return;
    }
    state.preview_note_id = selected_id;
    state.reset_preview_scroll();
    state.preview_body = match selected_id {
        Some(id) => storage
            .get_note(id)
            .await
            .ok()
            .flatten()
            .map(|n| n.body)
            .unwrap_or_default(),
        None => String::new(),
    };
    state.clear_code_focus();
    state.refresh_code_blocks();

    // Wikilinks: resolve this note's outgoing links to ids (for navigation),
    // build the live/dangling resolver set, and load backlinks. Not AI-gated.
    if state.settings.preview.wikilinks {
        let index = storage.live_title_index().await.unwrap_or_default();
        state.preview_links = crate::notes::wikilinks::extract_wikilinks(&state.preview_body)
            .into_iter()
            .map(|l| {
                let target = index
                    .get(&crate::notes::wikilinks::normalize_title(&l.target))
                    .copied();
                PreviewLink {
                    display: l.alias.unwrap_or(l.target),
                    target,
                }
            })
            .collect();
        state.link_targets = index.into_keys().collect();
        state.backlinks = match selected_id {
            Some(id) => storage.backlinks(id).await.unwrap_or_default(),
            None => Vec::new(),
        };
    } else {
        state.link_targets.clear();
        state.backlinks.clear();
        state.preview_links.clear();
    }

    // "On this day": prior daily notes sharing this daily note's calendar day.
    if state.settings.notes.on_this_day {
        let offsets = state.settings.notes.on_this_day_offsets.clone();
        state.on_this_day = match selected_id {
            Some(id) => storage
                .on_this_day_notes(id, &offsets)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
    } else {
        state.on_this_day.clear();
    }

    // Load related notes and tag suggestions when AI is available.
    #[cfg(feature = "ai")]
    if let Some(id) = selected_id {
        state.related_notes =
            storage.related_notes(id, 5).await.unwrap_or_default();
        state.suggested_tags = storage
            .suggest_tags_for_note(id, 8, 5)
            .await
            .unwrap_or_default();
    } else {
        state.related_notes.clear();
        state.suggested_tags.clear();
    }

    #[cfg(not(feature = "ai"))]
    {
        let _ = selected_id;
        state.related_notes.clear();
        state.suggested_tags.clear();
    }
}

/// Run a search using the active strategy. The semantic arm is compiled out
/// without the `ai` feature, so it transparently falls back to keyword search.
async fn run_search(
    storage: &SqliteStorage,
    query: &str,
    kind: SearchKind,
) -> Result<Vec<NoteSummary>> {
    match kind {
        #[cfg(feature = "ai")]
        SearchKind::Semantic => ai::retriever::hybrid_search(storage, query, 50).await,
        _ => storage.search_notes(query).await,
    }
}

/// Reload notes from storage and update the state.
async fn reload_notes(state: &mut AppState, storage: &SqliteStorage) {
    let notes = if let Some(ref tag) = state.tag_filter {
        storage
            .list_notes_by_tag(tag)
            .await
            .unwrap_or_else(|_| vec![])
    } else {
        storage.list_notes().await.unwrap_or_else(|_| vec![])
    };
    state.set_notes(notes);
    // Force the preview to reload — the selected note's body may have changed.
    state.preview_note_id = None;
}

/// Apply the Nth (0-based) suggested tag from the preview to the selected note.
/// No-op when the index is out of range (e.g. the digit has no suggestion).
async fn apply_suggested_tag(
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
    index: usize,
) {
    let Some(tag_name) = state.suggested_tags.get(index).cloned() else {
        return;
    };
    let Some(note) = state.selected_note().cloned() else {
        state.set_status("No note selected");
        return;
    };
    let (reply, rx) = oneshot::channel();
    if persist_tx
        .send(PersistenceCommand::AddTag {
            note_id: note.id,
            tag_name: tag_name.clone(),
            reply,
        })
        .await
        .is_ok()
    {
        match rx.await {
            // reload_notes clears preview_note_id, so the next refresh drops the
            // now-applied suggestion and recomputes related notes.
            Ok(Ok(_)) => {
                state.set_status(format!("Tagged #{}", tag_name));
                reload_notes(state, storage).await;
            }
            Ok(Err(e)) => state.set_status(format!("Error: {}", e)),
            Err(_) => state.set_status("Worker channel closed"),
        }
    }
}

/// Apply a conflict resolution strategy.
async fn resolve_conflict_action(
    state: &mut AppState,
    storage: &SqliteStorage,
    persist_tx: &mpsc::Sender<PersistenceCommand>,
    strategy: ConflictResolution,
) -> Result<bool> {
    let conflict = match state.selected_conflict().cloned() {
        Some(c) => c,
        None => {
            state.set_status("No conflict selected");
            return Ok(false);
        }
    };

    let conflict_id = conflict.id;
    let note_id = conflict.note_id;

    match strategy {
        ConflictResolution::KeepLocal => {
            // Mark conflict resolved — local note stays as-is
            if let Err(e) = storage
                .resolve_conflict(conflict_id, strategy.as_db_str())
                .await
            {
                state.set_status(format!("Failed to resolve conflict: {}", e));
                return Ok(false);
            }
            state.set_status("Kept local version");
        }
        ConflictResolution::KeepRemote => {
            // Overwrite local note with remote version
            let title = conflict.remote_payload["title"]
                .as_str()
                .unwrap_or("Untitled")
                .to_string();
            let body = conflict.remote_payload["body"]
                .as_str()
                .unwrap_or("")
                .to_string();

            let input = UpdateNoteInput {
                id: note_id,
                title,
                body,
            };

            let (reply, rx) = oneshot::channel();
            if persist_tx
                .send(PersistenceCommand::UpdateNote { input, reply })
                .await
                .is_err()
            {
                state.set_status("Worker channel closed");
                return Ok(false);
            }

            match rx.await {
                Ok(Ok(_)) => {
                    if let Err(e) = storage
                        .resolve_conflict(conflict_id, strategy.as_db_str())
                        .await
                    {
                        state.set_status(format!("Failed to resolve conflict: {}", e));
                        return Ok(false);
                    }
                    state.set_status("Kept remote version");
                }
                Ok(Err(e)) => {
                    state.set_status(format!("Save error: {}", e));
                    return Ok(false);
                }
                Err(_) => {
                    state.set_status("Worker channel closed");
                    return Ok(false);
                }
            }
        }
        ConflictResolution::SaveBoth => {
            // Keep local note, create a new note from remote payload
            let title = conflict.remote_payload["title"]
                .as_str()
                .unwrap_or("Untitled")
                .to_string();
            let body = conflict.remote_payload["body"]
                .as_str()
                .unwrap_or("")
                .to_string();

            let conflict_title = format!("{} (conflict copy)", title);
            let input = models::note::CreateNoteInput {
                title: conflict_title,
                body,
            };

            let (reply, rx) = oneshot::channel();
            if persist_tx
                .send(PersistenceCommand::CreateNote { input, reply })
                .await
                .is_err()
            {
                state.set_status("Worker channel closed");
                return Ok(false);
            }

            match rx.await {
                Ok(Ok(_)) => {
                    if let Err(e) = storage
                        .resolve_conflict(conflict_id, strategy.as_db_str())
                        .await
                    {
                        state.set_status(format!("Failed to resolve conflict: {}", e));
                        return Ok(false);
                    }
                    state.set_status("Saved both versions");
                }
                Ok(Err(e)) => {
                    state.set_status(format!("Save error: {}", e));
                    return Ok(false);
                }
                Err(_) => {
                    state.set_status("Worker channel closed");
                    return Ok(false);
                }
            }
        }
    }

    state.view = AppView::List;
    reload_notes(state, storage).await;
    if let Ok(count) = storage.count_unresolved_conflicts().await {
        state.conflict_count = count;
    }
    Ok(false)
}
