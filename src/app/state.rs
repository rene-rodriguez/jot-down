use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use ratatui::widgets::ListState;

use crate::app::commands::{self, CommandId};
use crate::config::Settings;
use crate::models::note::NoteSummary;
use crate::storage::sqlite::LocalConflict;
use uuid::Uuid;

/// The active view or mode within the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppView {
    /// Browsing the note list.
    List,
    /// Viewing a note preview.
    Preview,
    /// Typing a find-in-preview query (over the rendered preview).
    PreviewSearch,
    /// Searching notes.
    Search,
    /// Asking a question answered from the notes (RAG).
    Ask,
    /// Editing a note title.
    Edit,
    /// Adding a tag to a note.
    Tag,
    /// Removing a tag from a note.
    TagRemove,
    /// Filtering notes by tag.
    TagFilter,
    /// Full text editor for note body.
    Editor,
    /// In-note search (Ctrl+F in the editor).
    EditorSearch,
    /// Browsing unresolved sync conflicts.
    ConflictReview,
    /// Viewing a single conflict detail (keep-local/keep-remote/save-both).
    ConflictDetail,
    /// First-run welcome / settings editor (storage + sync configuration).
    Settings,
    /// Confirming deletion of the selected note (y/n).
    ConfirmDelete,
    /// Confirming a full rebuild of the embedding index (y/n). Only reachable
    /// in `ai` builds (entered from the `X` keybind).
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    ConfirmReindex,
    /// Scrollable keybinding reference overlay.
    Help,
    /// Browsing soft-deleted notes (the trash) to restore or purge them.
    Trash,
    /// Confirming a permanent purge of the selected trashed note (y/n).
    ConfirmPurge,
    /// Confirming execution of a focused code block from the preview (y/n).
    ConfirmRunBlock,
    /// Fuzzy command launcher.
    CommandPalette,
}

/// Loopback check for a base URL. Mirrors `crate::ai::chat::is_local` so the
/// settings form can warn about remote endpoints without depending on the
/// feature-gated ai module.
fn url_is_local(base_url: &str) -> bool {
    let after_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = host_port
        .strip_prefix('[')
        .and_then(|rest| rest.split(']').next())
        .unwrap_or_else(|| host_port.split(':').next().unwrap_or(""));
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
}

/// The kind of a settings field, which drives input vs. toggle/cycle handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Free text (some text fields accept digits only — see `input_char`).
    Text,
    /// On/off, flipped with Space or ←/→.
    Bool,
    /// One of a fixed set, cycled with Space or ←/→.
    Choice,
}

/// An editable form backing the Settings / first-run setup screen.
///
/// Field values are held as strings while editing and parsed/validated on save.
#[derive(Debug, Clone, Default)]
pub struct SettingsForm {
    pub data_dir: String,
    pub db_path: String,
    pub sync_enabled: bool,
    pub database_url: String,
    pub poll_interval: String,
    pub autosave: String,
    pub ai_enabled: bool,
    pub search_default: String,
    pub chat_base_url: String,
    pub chat_model: String,
    pub chat_api_key_env: String,
    pub chat_allow_remote: bool,
    // Preview settings
    pub preview_render_markdown: bool,
    pub preview_link_urls: String,
    pub preview_typographer: bool,
    pub preview_emoji: bool,
    pub preview_mark: bool,
    pub preview_ins: bool,
    pub preview_sup_sub: bool,
    pub preview_abbreviations: bool,
    pub preview_definition_lists: bool,
    pub preview_custom_containers: bool,
    pub preview_linkify: bool,
    pub preview_math: bool,
    /// Currently focused field (0..FIELD_COUNT).
    pub field: usize,
    /// True when shown as the first-run welcome (changes copy and Esc behaviour).
    pub first_run: bool,
    /// Inline feedback (validation / connection test result / save status).
    pub status: String,
}

impl SettingsForm {
    /// Number of editable rows in the form.
    pub const FIELD_COUNT: usize = 24;

    /// The kind of the field at `index`.
    pub fn field_kind(index: usize) -> FieldKind {
        match index {
            2 | 6 | 11 => FieldKind::Bool,
            7 => FieldKind::Choice,
            // Preview fields
            12 | 14 | 15 | 16 | 17 | 19 | 20 | 21 | 22 | 23 => FieldKind::Bool,
            13 => FieldKind::Choice,
            _ => FieldKind::Text,
        }
    }

    /// Build a form pre-populated from the current settings.
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            data_dir: s.data_dir.display().to_string(),
            db_path: s.db_path.display().to_string(),
            sync_enabled: s.sync.enabled,
            database_url: s.sync.database_url.clone().unwrap_or_default(),
            poll_interval: s.sync.poll_interval_seconds.to_string(),
            autosave: s.editor.autosave_seconds.to_string(),
            ai_enabled: s.ai.enabled,
            search_default: s.ai.search_default.clone(),
            chat_base_url: s.ai.chat.base_url.clone(),
            chat_model: s.ai.chat.model.clone(),
            chat_api_key_env: s.ai.chat.api_key_env.clone().unwrap_or_default(),
            chat_allow_remote: s.ai.chat.allow_remote,
            preview_render_markdown: s.preview.render_markdown,
            preview_link_urls: match s.preview.show_link_urls {
                crate::config::LinkUrlMode::Inline => "inline".to_string(),
                crate::config::LinkUrlMode::Footnote => "footnote".to_string(),
                crate::config::LinkUrlMode::Hide => "hide".to_string(),
            },
            preview_typographer: s.preview.typographer,
            preview_emoji: s.preview.emoji,
            preview_mark: s.preview.mark,
            preview_ins: s.preview.ins,
            preview_sup_sub: s.preview.sup_sub,
            preview_abbreviations: s.preview.abbreviations,
            preview_definition_lists: s.preview.definition_lists,
            preview_custom_containers: s.preview.custom_containers,
            preview_linkify: s.preview.linkify,
            preview_math: s.preview.math,
            field: 0,
            first_run: false,
            status: String::new(),
        }
    }

    /// Human label for each field index.
    pub fn label(index: usize) -> &'static str {
        match index {
            0 => "Data directory",
            1 => "Database file",
            2 => "Sync enabled",
            3 => "Postgres URL",
            4 => "Poll interval (s)",
            5 => "Autosave (s)",
            6 => "AI enabled",
            7 => "Default search",
            8 => "Chat base URL",
            9 => "Chat model",
            10 => "API key env var",
            11 => "Allow remote AI",
            // Preview section
            12 => "Markdown render",
            13 => "Link URLs",
            14 => "Typographer",
            15 => "Emoji",
            16 => "==mark== highlight",
            17 => "++ins++ underline",
            18 => "Sup/subscript",
            19 => "Abbreviations",
            20 => "Definition lists",
            21 => "Custom containers",
            22 => "Linkify URLs",
            23 => "Unicode math",
            _ => "",
        }
    }

    /// Current display value for a field index.
    pub fn value(&self, index: usize) -> String {
        let checkbox = |on: bool| if on { "[x]" } else { "[ ]" }.to_string();
        match index {
            0 => self.data_dir.clone(),
            1 => self.db_path.clone(),
            2 => checkbox(self.sync_enabled),
            3 => self.database_url.clone(),
            4 => self.poll_interval.clone(),
            5 => self.autosave.clone(),
            6 => checkbox(self.ai_enabled),
            7 => self.search_default.clone(),
            8 => self.chat_base_url.clone(),
            9 => self.chat_model.clone(),
            10 => self.chat_api_key_env.clone(),
            11 => checkbox(self.chat_allow_remote),
            12 => checkbox(self.preview_render_markdown),
            13 => self.preview_link_urls.clone(),
            14 => checkbox(self.preview_typographer),
            15 => checkbox(self.preview_emoji),
            16 => checkbox(self.preview_mark),
            17 => checkbox(self.preview_ins),
            18 => checkbox(self.preview_sup_sub),
            19 => checkbox(self.preview_abbreviations),
            20 => checkbox(self.preview_definition_lists),
            21 => checkbox(self.preview_custom_containers),
            22 => checkbox(self.preview_linkify),
            23 => checkbox(self.preview_math),
            _ => String::new(),
        }
    }

    pub fn next_field(&mut self) {
        self.field = (self.field + 1) % Self::FIELD_COUNT;
    }

    pub fn prev_field(&mut self) {
        self.field = if self.field == 0 {
            Self::FIELD_COUNT - 1
        } else {
            self.field - 1
        };
    }

    /// True when the focused field is toggled/cycled rather than typed into.
    pub fn on_toggle_field(&self) -> bool {
        !matches!(Self::field_kind(self.field), FieldKind::Text)
    }

    /// Flip a bool field or cycle a choice field (the focused one).
    pub fn toggle_or_cycle(&mut self) {
        match self.field {
            2 => self.sync_enabled = !self.sync_enabled,
            6 => self.ai_enabled = !self.ai_enabled,
            11 => {
                self.chat_allow_remote = !self.chat_allow_remote;
                self.update_remote_warning();
            }
            7 => {
                self.search_default = if self.search_default.eq_ignore_ascii_case("semantic") {
                    "keyword".to_string()
                } else {
                    "semantic".to_string()
                };
            }
            // Preview toggles
            12 => self.preview_render_markdown = !self.preview_render_markdown,
            13 => {
                self.preview_link_urls = match self.preview_link_urls.as_str() {
                    "inline" => "footnote".to_string(),
                    "footnote" => "hide".to_string(),
                    _ => "inline".to_string(),
                };
            }
            14 => self.preview_typographer = !self.preview_typographer,
            15 => self.preview_emoji = !self.preview_emoji,
            16 => self.preview_mark = !self.preview_mark,
            17 => self.preview_ins = !self.preview_ins,
            18 => self.preview_sup_sub = !self.preview_sup_sub,
            19 => self.preview_abbreviations = !self.preview_abbreviations,
            20 => self.preview_definition_lists = !self.preview_definition_lists,
            21 => self.preview_custom_containers = !self.preview_custom_containers,
            22 => self.preview_linkify = !self.preview_linkify,
            23 => self.preview_math = !self.preview_math,
            _ => {}
        }
    }

    /// Append a typed character to the focused text field.
    pub fn input_char(&mut self, c: char) {
        match self.field {
            0 => self.data_dir.push(c),
            1 => self.db_path.push(c),
            3 => self.database_url.push(c),
            // Numeric fields only accept digits.
            4 if c.is_ascii_digit() => self.poll_interval.push(c),
            5 if c.is_ascii_digit() => self.autosave.push(c),
            8 => {
                self.chat_base_url.push(c);
                self.update_remote_warning();
            }
            9 => self.chat_model.push(c),
            10 => self.chat_api_key_env.push(c),
            // Preview text fields (only link_urls right now is Choice, not text)
            // 2, 6, 7, 11-22 are toggle/choice fields — handled via toggle_or_cycle.
            _ => {}
        }
    }

    /// Delete the last character of the focused text field.
    pub fn backspace(&mut self) {
        match self.field {
            0 => drop(self.data_dir.pop()),
            1 => drop(self.db_path.pop()),
            3 => drop(self.database_url.pop()),
            4 => drop(self.poll_interval.pop()),
            5 => drop(self.autosave.pop()),
            8 => {
                self.chat_base_url.pop();
                self.update_remote_warning();
            }
            9 => drop(self.chat_model.pop()),
            10 => drop(self.chat_api_key_env.pop()),
            _ => {}
        }
    }

    /// Set or clear the privacy warning shown when a remote AI endpoint is
    /// allowed. Mirrors `crate::ai::chat::is_local` without depending on the
    /// (feature-gated) ai module.
    fn update_remote_warning(&mut self) {
        if self.chat_allow_remote && !url_is_local(&self.chat_base_url) {
            self.status = "⚠ Remote AI endpoint — note content will be sent off-device".to_string();
        } else if self.status.starts_with("⚠ Remote AI") {
            self.status.clear();
        }
    }

    /// Validate and apply the form into a Settings struct.
    pub fn apply_to(&self, s: &mut Settings) -> Result<(), String> {
        let poll = self
            .poll_interval
            .trim()
            .parse::<u64>()
            .map_err(|_| "Poll interval must be a whole number of seconds".to_string())?;
        let autosave = self
            .autosave
            .trim()
            .parse::<u64>()
            .map_err(|_| "Autosave must be a whole number of seconds".to_string())?;

        if self.data_dir.trim().is_empty() {
            return Err("Data directory cannot be empty".to_string());
        }
        if self.db_path.trim().is_empty() {
            return Err("Database file path cannot be empty".to_string());
        }
        if self.sync_enabled && self.database_url.trim().is_empty() {
            return Err("Enable sync requires a Postgres URL (or disable sync)".to_string());
        }

        s.data_dir = PathBuf::from(self.data_dir.trim());
        s.db_path = PathBuf::from(self.db_path.trim());
        s.sync.enabled = self.sync_enabled;
        s.sync.database_url = {
            let u = self.database_url.trim();
            if u.is_empty() {
                None
            } else {
                Some(u.to_string())
            }
        };
        s.sync.poll_interval_seconds = poll;
        s.editor.autosave_seconds = autosave;

        s.ai.enabled = self.ai_enabled;
        s.ai.search_default = if self.search_default.eq_ignore_ascii_case("keyword") {
            "keyword".to_string()
        } else {
            "semantic".to_string()
        };
        s.ai.chat.base_url = {
            let url = self.chat_base_url.trim();
            if url.is_empty() {
                return Err("Chat base URL cannot be empty (or disable AI)".to_string());
            }
            url.to_string()
        };
        s.ai.chat.model = self.chat_model.trim().to_string();
        s.ai.chat.api_key_env = {
            let var = self.chat_api_key_env.trim();
            if var.is_empty() {
                None
            } else {
                Some(var.to_string())
            }
        };
        s.ai.chat.allow_remote = self.chat_allow_remote;

        s.preview.render_markdown = self.preview_render_markdown;
        s.preview.show_link_urls = match self.preview_link_urls.as_str() {
            "footnote" => crate::config::LinkUrlMode::Footnote,
            "hide" => crate::config::LinkUrlMode::Hide,
            _ => crate::config::LinkUrlMode::Inline,
        };
        s.preview.typographer = self.preview_typographer;
        s.preview.emoji = self.preview_emoji;
        s.preview.mark = self.preview_mark;
        s.preview.ins = self.preview_ins;
        s.preview.sup_sub = self.preview_sup_sub;
        s.preview.abbreviations = self.preview_abbreviations;
        s.preview.definition_lists = self.preview_definition_lists;
        s.preview.custom_containers = self.preview_custom_containers;
        s.preview.linkify = self.preview_linkify;
        s.preview.math = self.preview_math;
        Ok(())
    }
}

/// Kind of body edit, used to coalesce consecutive same-kind edits into one
/// undo step (a run of insertions undoes together; a run of deletions undoes
/// together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
}

/// A point-in-time snapshot of the body editor for undo/redo.
#[derive(Debug, Clone)]
struct EditSnapshot {
    body: String,
    cursor: usize,
}

/// Maximum number of undo steps retained for the body editor.
const MAX_UNDO_DEPTH: usize = 200;

/// Which search strategy the search bar uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    /// Substring match over title and body.
    Keyword,
    /// Meaning-based vector search (hybrid with keyword), falling back to
    /// keyword when the vector index is unavailable.
    Semantic,
}

impl SearchKind {
    /// Short label for the search bar.
    pub fn label(self) -> &'static str {
        match self {
            SearchKind::Keyword => "keyword",
            SearchKind::Semantic => "semantic",
        }
    }
}

/// A `[[wikilink]]` in the previewed note, in document order, resolved to a
/// target note id where one exists (`None` = dangling). Drives Enter-to-open
/// navigation and aligns index-for-index with the renderer's masked link table.
#[derive(Debug, Clone)]
pub struct PreviewLink {
    pub display: String,
    pub target: Option<Uuid>,
}

/// What the preview's focus cursor currently points at. Code blocks, inline
/// wikilinks, and panel entries (backlinks / "On this day") share one cycle
/// (`]`/`[`/Tab); the linear order is "code blocks, then inline links, then
/// panel links".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewFocus {
    Code(usize),
    Link(usize),
    Panel(usize),
}

/// Active `[[` autocomplete session in the body editor.
#[derive(Debug, Clone)]
pub struct WikiComplete {
    /// Byte offset just after the opening `[[` — the start of the text the
    /// accepted title replaces.
    pub start: usize,
    /// The partial title typed after `[[` (for display/highlight).
    pub query: String,
    /// Fuzzy-matched candidate note titles, best first.
    pub matches: Vec<String>,
    /// Index of the highlighted candidate.
    pub selected: usize,
}

/// Central application state.
#[derive(Debug, Clone)]
pub struct AppState {
    /// All loaded notes (filtered by current search if any).
    pub notes: Vec<NoteSummary>,
    /// The current search query, empty if not searching.
    pub search_query: String,
    /// Active search strategy (keyword vs semantic).
    pub search_kind: SearchKind,
    /// Text buffer used for title editing and tag input.
    pub edit_buffer: String,
    /// The active view.
    pub view: AppView,
    /// Status message shown in the status bar.
    pub status_message: String,
    /// Whether the app should exit.
    pub should_quit: bool,
    /// The state of the ratatui List widget (tracks selection).
    pub list_state: ListState,
    /// Full body text being edited in the editor view.
    pub body_buffer: String,
    /// Cursor position within body_buffer (byte index).
    pub cursor_pos: usize,
    /// Active `[[` wikilink autocomplete in the editor, if any.
    pub wiki_complete: Option<WikiComplete>,
    /// Vertical scroll offset (in wrapped visual rows) of the body editor.
    /// View-only state: updated during rendering to keep the cursor on screen,
    /// hence `Cell` so it can be adjusted through a shared `&AppState`.
    pub editor_scroll: Cell<u16>,
    /// When true, the body editor renders a live markdown preview beside it
    /// (toggled with Ctrl+P). See `render_editor_preview` in the layout.
    pub editor_preview_split: bool,
    /// Vertical scroll offset (in rendered rows) of the editor's live preview.
    /// Driven to follow the editor cursor's section; `Cell` for shared-ref use.
    pub editor_preview_scroll: Cell<u16>,
    /// Memoized render of `body_buffer` for the live preview, keyed by
    /// (body hash, inner width). Avoids re-parsing markdown on every keystroke.
    pub editor_preview_cache: RefCell<Option<(u64, u16, crate::tui::markdown::RenderOutput)>>,
    /// In-note find query (active in AppView::EditorSearch).
    pub editor_search_query: String,
    /// Byte offsets of current matches in body_buffer, ascending.
    pub editor_search_matches: Vec<usize>,
    /// Index into editor_search_matches of the current match.
    pub editor_search_idx: usize,
    /// Cursor position when find was opened; nearest match is chosen relative to it.
    editor_search_anchor: usize,
    /// Undo history for the body editor (snapshots before each edit group).
    undo_stack: Vec<EditSnapshot>,
    /// Redo history (snapshots undone, restorable until the next fresh edit).
    redo_stack: Vec<EditSnapshot>,
    /// Kind of the in-progress edit group, for coalescing (None after a
    /// navigation, undo, or redo, so the next edit starts a fresh step).
    last_edit_kind: Option<EditKind>,
    /// Vertical scroll offset (in rendered rows) of the preview pane.
    pub preview_scroll: Cell<u16>,
    /// Last rendered preview viewport height, used for page-size scrolling.
    pub preview_viewport_height: Cell<u16>,
    /// Whether the preview pane renders markdown or shows the raw body.
    pub preview_render_markdown: bool,
    /// Active tag filter — when Some, only notes with this tag are shown.
    pub tag_filter: Option<String>,
    /// Whether PostgreSQL sync is enabled and connected.
    pub sync_enabled: bool,
    /// Current sync status message (e.g. "Synced", "Syncing...", "Offline").
    pub sync_status: String,
    /// Unresolved conflicts (loaded from SQLite when reviewing).
    pub conflicts: Vec<LocalConflict>,
    /// Index into conflicts for the ConflictReview list.
    pub conflict_index: usize,
    /// Soft-deleted notes loaded when browsing the Trash view.
    pub trash_notes: Vec<NoteSummary>,
    /// Selection state for the Trash list.
    pub trash_state: ListState,
    /// Current query text in the command palette.
    pub palette_query: String,
    /// Commands matching the palette query, best match first.
    pub palette_matches: Vec<CommandId>,
    /// Selection state for the palette list.
    pub palette_state: ListState,
    /// Cached count of unresolved conflicts for the status-bar indicator,
    /// refreshed after each sync without disturbing the review list.
    pub conflict_count: usize,
    /// The live application settings (used to seed and persist the form).
    pub settings: Settings,
    /// Editable settings form backing the Settings view.
    pub settings_form: SettingsForm,
    /// Full body of the currently previewed note, loaded lazily on selection.
    pub preview_body: String,
    /// Which note `preview_body` was loaded for (to avoid redundant reloads).
    pub preview_note_id: Option<uuid::Uuid>,
    /// The question being typed in the Ask view.
    pub ask_input: String,
    /// True while an answer is being generated.
    pub ask_pending: bool,
    /// The latest answer text (markdown), if any.
    pub ask_answer: Option<String>,
    /// Citations backing the latest answer.
    pub ask_citations: Vec<crate::workers::AskCitation>,
    /// Notes still pending embedding (for the status-bar index indicator).
    pub embed_pending: usize,
    /// Semantically related notes for the currently previewed note.
    pub related_notes: Vec<NoteSummary>,
    /// Notes that link *to* the currently previewed note via `[[wikilinks]]`.
    pub backlinks: Vec<NoteSummary>,
    /// "On this day" daily notes from prior periods, as `(offset_days, note)`.
    pub on_this_day: Vec<(u32, NoteSummary)>,
    /// Normalized titles of all live notes, for resolving `[[wikilinks]]` as
    /// live vs dangling in the preview. Refreshed when the preview reloads.
    pub link_targets: std::collections::HashSet<String>,
    /// Auto-tag suggestions for the currently previewed note.
    pub suggested_tags: Vec<String>,
    /// Fenced code blocks of the currently previewed note (for code actions).
    pub preview_code_blocks: Vec<crate::tui::markdown::CodeBlock>,
    /// Outgoing `[[wikilinks]]` of the previewed note, in document order.
    pub preview_links: Vec<PreviewLink>,
    /// The preview's focus cursor (code block or link), shared across both.
    pub preview_focus: Option<PreviewFocus>,
    /// When set, the next preview render scrolls the focused code block into
    /// view. Cleared by the renderer once honored. View-only, hence `Cell`.
    pub preview_focus_scroll: Cell<bool>,
    /// Headings of the last-rendered preview, with absolute preview rows. Powers
    /// the outline overlay, jump-to-heading, and scrollbar ticks. Refreshed each
    /// render, hence `RefCell` for interior mutability through a shared ref.
    pub preview_headings: RefCell<Vec<crate::tui::markdown::Heading>>,
    /// Whether the outline (jump-to-heading) overlay is open over the preview.
    pub outline_open: bool,
    /// Selected heading index in the outline overlay.
    pub outline_selected: usize,
    /// Find-in-preview query (matched case-insensitively against rendered text).
    /// Stays set after confirming so `n`/`N` keep working in the preview.
    pub preview_search_query: String,
    /// Matches of the last render: (output row, char start, char end). Refreshed
    /// each render so `n`/`N` can navigate against current positions.
    pub preview_search_matches: RefCell<Vec<(usize, usize, usize)>>,
    /// Index of the current find-in-preview match.
    pub preview_search_idx: usize,
    /// When set, the next preview render scrolls the current match into view.
    /// Honored once by the renderer. View-only, hence `Cell`.
    pub preview_search_scroll: Cell<bool>,
    /// Heading indices whose sections are folded (collapsed) in the preview.
    /// Cleared when the previewed note changes. Index-stable because folding
    /// collapses content but never removes a heading from the rendered list.
    pub folded_headings: RefCell<std::collections::HashSet<usize>>,
    /// Whether the preview renders in zen mode: a narrower, centered reading
    /// column (width from `settings.preview.zen_width`).
    pub zen_mode: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            notes: Vec::new(),
            search_query: String::new(),
            search_kind: SearchKind::Keyword,
            edit_buffer: String::new(),
            view: AppView::List,
            status_message: "Ready".to_string(),
            should_quit: false,
            list_state: ListState::default(),
            body_buffer: String::new(),
            cursor_pos: 0,
            wiki_complete: None,
            editor_scroll: Cell::new(0),
            editor_preview_split: false,
            editor_preview_scroll: Cell::new(0),
            editor_preview_cache: RefCell::new(None),
            editor_search_query: String::new(),
            editor_search_matches: Vec::new(),
            editor_search_idx: 0,
            editor_search_anchor: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit_kind: None,
            preview_scroll: Cell::new(0),
            preview_viewport_height: Cell::new(1),
            preview_render_markdown: true,
            tag_filter: None,
            sync_enabled: false,
            sync_status: "local".to_string(),
            conflicts: Vec::new(),
            conflict_index: 0,
            trash_notes: Vec::new(),
            trash_state: ListState::default(),
            palette_query: String::new(),
            palette_matches: Vec::new(),
            palette_state: ListState::default(),
            conflict_count: 0,
            settings: Settings::default(),
            settings_form: SettingsForm::default(),
            preview_body: String::new(),
            preview_note_id: None,
            ask_input: String::new(),
            ask_pending: false,
            ask_answer: None,
            ask_citations: Vec::new(),
            embed_pending: 0,
            related_notes: Vec::new(),
            backlinks: Vec::new(),
            on_this_day: Vec::new(),
            link_targets: std::collections::HashSet::new(),
            suggested_tags: Vec::new(),
            preview_code_blocks: Vec::new(),
            preview_links: Vec::new(),
            preview_focus: None,
            preview_focus_scroll: Cell::new(false),
            preview_headings: RefCell::new(Vec::new()),
            outline_open: false,
            outline_selected: 0,
            preview_search_query: String::new(),
            preview_search_matches: RefCell::new(Vec::new()),
            preview_search_idx: 0,
            preview_search_scroll: Cell::new(false),
            folded_headings: RefCell::new(std::collections::HashSet::new()),
            zen_mode: false,
        }
    }

    /// Toggle zen (centered reading column) mode in the preview.
    pub fn toggle_zen_mode(&mut self) {
        self.zen_mode = !self.zen_mode;
        let msg = if self.zen_mode {
            "Zen reading on — w to exit"
        } else {
            "Zen reading off"
        };
        self.set_status(msg);
    }

    /// Toggle the fold of the section whose heading sits at or above the current
    /// scroll. No-op when the previewed note has no headings.
    pub fn toggle_fold_at_scroll(&mut self) {
        if self.preview_headings.borrow().is_empty() {
            self.set_status("No headings to fold");
            return;
        }
        let idx = self.nearest_heading_to_scroll();
        let mut folds = self.folded_headings.borrow_mut();
        if !folds.insert(idx) {
            folds.remove(&idx);
        }
    }

    /// Fold every section, or unfold all if any are already folded.
    pub fn toggle_fold_all(&mut self) {
        let count = self.preview_headings.borrow().len();
        if count == 0 {
            self.set_status("No headings to fold");
            return;
        }
        let mut folds = self.folded_headings.borrow_mut();
        if folds.is_empty() {
            *folds = (0..count).collect();
        } else {
            folds.clear();
        }
    }

    /// Open find-in-preview: switch to the query input and start fresh.
    pub fn open_preview_search(&mut self) {
        self.view = AppView::PreviewSearch;
        self.preview_search_query.clear();
        self.preview_search_idx = 0;
        self.preview_search_matches.borrow_mut().clear();
    }

    /// Append a character to the find query and re-seek from the first match.
    pub fn preview_search_input(&mut self, c: char) {
        self.preview_search_query.push(c);
        self.preview_search_idx = 0;
        self.preview_search_scroll.set(true);
    }

    /// Delete the last character of the find query.
    pub fn preview_search_backspace(&mut self) {
        self.preview_search_query.pop();
        self.preview_search_idx = 0;
        self.preview_search_scroll.set(true);
    }

    /// Confirm the find: keep the query highlighted and return to the preview so
    /// `n`/`N` can step through matches.
    pub fn confirm_preview_search(&mut self) {
        self.view = AppView::Preview;
    }

    /// Cancel the find: clear the query and matches, return to the preview.
    pub fn cancel_preview_search(&mut self) {
        self.preview_search_query.clear();
        self.preview_search_matches.borrow_mut().clear();
        self.preview_search_idx = 0;
        self.view = AppView::Preview;
    }

    /// Step to the next (`delta = 1`) or previous (`delta = -1`) find match,
    /// wrapping around. No-op when there are no matches.
    pub fn preview_search_step(&mut self, delta: i32) {
        let total = self.preview_search_matches.borrow().len();
        if total == 0 {
            return;
        }
        let cur = self.preview_search_idx.min(total - 1) as i32;
        self.preview_search_idx = (cur + delta).rem_euclid(total as i32) as usize;
        self.preview_search_scroll.set(true);
    }

    /// Open the outline (jump-to-heading) overlay, selecting the heading nearest
    /// at or above the current scroll. No-op if the note has no headings.
    pub fn open_outline(&mut self) {
        if self.preview_headings.borrow().is_empty() {
            self.set_status("No headings in this note");
            return;
        }
        self.outline_selected = self.nearest_heading_to_scroll();
        self.outline_open = true;
    }

    /// Close the outline overlay.
    pub fn close_outline(&mut self) {
        self.outline_open = false;
    }

    /// Move the outline selection by `delta`, clamped to the heading range.
    pub fn outline_move(&mut self, delta: i32) {
        let count = self.preview_headings.borrow().len();
        if count == 0 {
            return;
        }
        let cur = self.outline_selected.min(count - 1) as i32;
        self.outline_selected = (cur + delta).clamp(0, count as i32 - 1) as usize;
    }

    /// Index of the heading at or above the current preview scroll position.
    fn nearest_heading_to_scroll(&self) -> usize {
        let scroll = usize::from(self.preview_scroll.get());
        self.preview_headings
            .borrow()
            .iter()
            .rposition(|h| h.row <= scroll)
            .unwrap_or(0)
    }

    /// Scroll the preview so the selected outline heading sits near the top.
    pub fn jump_to_selected_heading(&self) {
        let headings = self.preview_headings.borrow();
        if let Some(h) = headings.get(self.outline_selected) {
            let row = h.row.saturating_sub(1).min(usize::from(u16::MAX)) as u16;
            self.preview_scroll.set(row);
        }
    }

    /// Re-extract the previewed note's code blocks and drop an out-of-range
    /// focus. Call whenever `preview_body` changes.
    pub fn refresh_code_blocks(&mut self) {
        self.preview_code_blocks = crate::tui::markdown::extract_code_blocks(&self.preview_body);
        if !self.focus_in_range() {
            self.preview_focus = None;
        }
        // Fold state is keyed by heading index, which only stays meaningful for
        // one note's headings; drop it when the previewed body changes.
        self.folded_headings.borrow_mut().clear();
    }

    /// Clear the preview focus cursor (code block or link).
    pub fn clear_code_focus(&mut self) {
        self.preview_focus = None;
    }

    /// Number of focusable panel entries: backlinks then "On this day".
    fn panel_link_count(&self) -> usize {
        self.backlinks.len() + self.on_this_day.len()
    }

    /// The (title, target id) of panel entry `i` — backlinks then on-this-day.
    fn panel_link(&self, i: usize) -> Option<(String, Option<Uuid>)> {
        let b = self.backlinks.len();
        if i < b {
            let n = &self.backlinks[i];
            Some((n.title.clone(), Some(n.id)))
        } else {
            self.on_this_day
                .get(i - b)
                .map(|(_, n)| (n.title.clone(), Some(n.id)))
        }
    }

    /// Number of focusable items: code blocks, then inline links, then panels.
    fn focusable_count(&self) -> usize {
        self.preview_code_blocks.len() + self.preview_links.len() + self.panel_link_count()
    }

    /// True if the current focus (if any) still points at a valid item.
    fn focus_in_range(&self) -> bool {
        match self.preview_focus {
            Some(PreviewFocus::Code(i)) => i < self.preview_code_blocks.len(),
            Some(PreviewFocus::Link(i)) => i < self.preview_links.len(),
            Some(PreviewFocus::Panel(i)) => i < self.panel_link_count(),
            None => true,
        }
    }

    /// Map the current focus to a linear index over `[code…, links…, panel…]`.
    fn focus_linear(&self) -> Option<usize> {
        let c = self.preview_code_blocks.len();
        let l = self.preview_links.len();
        match self.preview_focus {
            Some(PreviewFocus::Code(i)) => Some(i),
            Some(PreviewFocus::Link(i)) => Some(c + i),
            Some(PreviewFocus::Panel(i)) => Some(c + l + i),
            None => None,
        }
    }

    /// Set focus from a linear index, requesting a scroll-to for code blocks
    /// (which have row tracking; links/panels don't scroll).
    fn set_focus_linear(&mut self, n: usize) {
        let c = self.preview_code_blocks.len();
        let l = self.preview_links.len();
        self.preview_focus = Some(if n < c {
            self.preview_focus_scroll.set(true);
            PreviewFocus::Code(n)
        } else if n < c + l {
            PreviewFocus::Link(n - c)
        } else {
            PreviewFocus::Panel(n - c - l)
        });
    }

    /// Focus the next preview item — code block or link — wrapping around. The
    /// historical name is kept; it now cycles the shared focusable set. No-op
    /// when there's nothing focusable.
    pub fn focus_next_code_block(&mut self) {
        let total = self.focusable_count();
        if total == 0 {
            return;
        }
        let next = match self.focus_linear() {
            Some(i) => (i + 1) % total,
            None => 0,
        };
        self.set_focus_linear(next);
    }

    /// Focus the previous preview item (wrapping).
    pub fn focus_prev_code_block(&mut self) {
        let total = self.focusable_count();
        if total == 0 {
            return;
        }
        let prev = match self.focus_linear() {
            Some(0) | None => total - 1,
            Some(i) => i - 1,
        };
        self.set_focus_linear(prev);
    }

    /// Index of the focused code block, if a code block is focused.
    pub fn focused_code_index(&self) -> Option<usize> {
        match self.preview_focus {
            Some(PreviewFocus::Code(i)) => Some(i),
            _ => None,
        }
    }

    /// The currently focused code block, if any.
    pub fn focused_code_block(&self) -> Option<&crate::tui::markdown::CodeBlock> {
        self.focused_code_index()
            .and_then(|i| self.preview_code_blocks.get(i))
    }

    /// Index of the focused wikilink, if a link is focused (for render highlight).
    pub fn focused_link_index(&self) -> Option<usize> {
        match self.preview_focus {
            Some(PreviewFocus::Link(i)) => Some(i),
            _ => None,
        }
    }

    /// Index of the focused panel entry (backlink / on-this-day), if any —
    /// used by the layout to reverse-highlight it.
    pub fn focused_panel_index(&self) -> Option<usize> {
        match self.preview_focus {
            Some(PreviewFocus::Panel(i)) => Some(i),
            _ => None,
        }
    }

    /// The (display, target id) of whatever navigable item is focused — an
    /// inline wikilink or a panel entry. `None` when nothing navigable is
    /// focused (e.g. a code block, or no focus). Drives Enter-to-open.
    pub fn focused_nav_target(&self) -> Option<(String, Option<Uuid>)> {
        match self.preview_focus {
            Some(PreviewFocus::Link(i)) => {
                self.preview_links.get(i).map(|l| (l.display.clone(), l.target))
            }
            Some(PreviewFocus::Panel(i)) => self.panel_link(i),
            _ => None,
        }
    }

    /// Open the Ask view with a fresh question.
    pub fn open_ask(&mut self) {
        self.ask_input.clear();
        self.ask_answer = None;
        self.ask_citations.clear();
        self.ask_pending = false;
        self.view = AppView::Ask;
    }

    /// Open the settings editor, seeding the form from the current settings.
    pub fn open_settings(&mut self, first_run: bool) {
        let mut form = SettingsForm::from_settings(&self.settings);
        form.first_run = first_run;
        self.settings_form = form;
        self.view = AppView::Settings;
    }

    /// Open the in-note find view (Ctrl+F in the editor).
    pub fn open_editor_search(&mut self) {
        self.editor_search_query.clear();
        self.editor_search_matches.clear();
        self.editor_search_idx = 0;
        self.editor_search_anchor = self.cursor_pos;
        self.view = AppView::EditorSearch;
    }

    /// Recompute matches for the current query and jump to the first match at or
    /// after the anchor (wrapping). No-op when there are no matches.
    pub fn editor_search_refresh(&mut self) {
        self.editor_search_matches =
            find_matches(&self.body_buffer, &self.editor_search_query);
        if self.editor_search_matches.is_empty() {
            return;
        }
        self.editor_search_idx = self
            .editor_search_matches
            .iter()
            .position(|&off| off >= self.editor_search_anchor)
            .unwrap_or(0);
        self.cursor_pos = self.editor_search_matches[self.editor_search_idx];
    }

    /// Jump to the next match (wrapping).
    pub fn editor_search_next(&mut self) {
        self.editor_search_step(1);
    }

    /// Jump to the previous match (wrapping).
    pub fn editor_search_prev(&mut self) {
        self.editor_search_step(-1);
    }

    fn editor_search_step(&mut self, delta: isize) {
        let n = self.editor_search_matches.len();
        if n == 0 {
            return;
        }
        self.editor_search_idx =
            (self.editor_search_idx as isize + delta).rem_euclid(n as isize) as usize;
        self.cursor_pos = self.editor_search_matches[self.editor_search_idx];
    }

    /// Get the currently selected note summary, if any.
    pub fn selected_note(&self) -> Option<&NoteSummary> {
        self.list_state.selected().and_then(|i| self.notes.get(i))
    }

    /// Select the next note in the list, wrapping around.
    pub fn select_next(&mut self) {
        if self.notes.is_empty() {
            return;
        }
        let i = self
            .list_state
            .selected()
            .map(|i| (i + 1) % self.notes.len())
            .unwrap_or(0);
        self.list_state.select(Some(i));
    }

    /// Select the previous note in the list, wrapping around.
    pub fn select_previous(&mut self) {
        if self.notes.is_empty() {
            return;
        }
        let new_index = match self.list_state.selected() {
            Some(0) | None => self.notes.len() - 1,
            Some(i) => i - 1,
        };
        self.list_state.select(Some(new_index));
    }

    /// Set notes and reset selection if out of bounds.
    pub fn set_notes(&mut self, notes: Vec<NoteSummary>) {
        self.notes = notes;
        if self.notes.is_empty() {
            self.list_state.select(None);
        } else {
            let current = self.list_state.selected().unwrap_or(0);
            self.list_state
                .select(Some(current.min(self.notes.len() - 1)));
        }
    }

    /// Load notes into the Trash view, selecting the first if any.
    pub fn set_trash_notes(&mut self, notes: Vec<NoteSummary>) {
        self.trash_notes = notes;
        self.trash_state
            .select((!self.trash_notes.is_empty()).then_some(0));
    }

    /// The currently selected trashed note, if any.
    pub fn selected_trash_note(&self) -> Option<&NoteSummary> {
        self.trash_state.selected().and_then(|i| self.trash_notes.get(i))
    }

    /// Move the Trash selection by one, wrapping around.
    pub fn trash_select_next(&mut self) {
        if self.trash_notes.is_empty() {
            return;
        }
        let i = self
            .trash_state
            .selected()
            .map(|i| (i + 1) % self.trash_notes.len())
            .unwrap_or(0);
        self.trash_state.select(Some(i));
    }

    /// Move the Trash selection back one, wrapping around.
    pub fn trash_select_previous(&mut self) {
        if self.trash_notes.is_empty() {
            return;
        }
        let i = match self.trash_state.selected() {
            Some(0) | None => self.trash_notes.len() - 1,
            Some(i) => i - 1,
        };
        self.trash_state.select(Some(i));
    }

    /// Open the command palette with an empty query (all commands listed).
    pub fn open_palette(&mut self) {
        self.palette_query.clear();
        self.refresh_palette();
        self.view = AppView::CommandPalette;
        self.set_status("Command palette — type to filter, ↑/↓ move, Enter run, Esc close");
    }

    /// Append a character to the palette query and re-filter.
    pub fn palette_input(&mut self, c: char) {
        self.palette_query.push(c);
        self.refresh_palette();
    }

    /// Delete the last character of the palette query and re-filter.
    pub fn palette_backspace(&mut self) {
        self.palette_query.pop();
        self.refresh_palette();
    }

    /// Recompute matches for the current query, selecting the top result.
    fn refresh_palette(&mut self) {
        self.palette_matches = commands::filter_commands(&self.palette_query);
        self.palette_state
            .select((!self.palette_matches.is_empty()).then_some(0));
    }

    /// Move the palette selection down, wrapping around.
    pub fn palette_select_next(&mut self) {
        if self.palette_matches.is_empty() {
            return;
        }
        let i = self
            .palette_state
            .selected()
            .map(|i| (i + 1) % self.palette_matches.len())
            .unwrap_or(0);
        self.palette_state.select(Some(i));
    }

    /// Move the palette selection up, wrapping around.
    pub fn palette_select_previous(&mut self) {
        if self.palette_matches.is_empty() {
            return;
        }
        let i = match self.palette_state.selected() {
            Some(0) | None => self.palette_matches.len() - 1,
            Some(i) => i - 1,
        };
        self.palette_state.select(Some(i));
    }

    /// The currently highlighted palette command, if any.
    pub fn selected_palette_command(&self) -> Option<CommandId> {
        self.palette_state
            .selected()
            .and_then(|i| self.palette_matches.get(i).copied())
    }

    /// Update the status message.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = message.into();
    }

    /// Toggle between keyword and semantic search.
    pub fn toggle_search_kind(&mut self) {
        self.search_kind = match self.search_kind {
            SearchKind::Keyword => SearchKind::Semantic,
            SearchKind::Semantic => SearchKind::Keyword,
        };
    }

    /// The configured default search strategy (from `[ai].search_default`).
    pub fn default_search_kind(&self) -> SearchKind {
        if self
            .settings
            .ai
            .search_default
            .eq_ignore_ascii_case("semantic")
        {
            SearchKind::Semantic
        } else {
            SearchKind::Keyword
        }
    }

    /// Reset the preview scroll to the top.
    pub fn reset_preview_scroll(&self) {
        self.preview_scroll.set(0);
    }

    /// Scroll the preview by a signed number of rendered rows.
    pub fn scroll_preview_lines(&self, delta: i16) {
        let current = self.preview_scroll.get();
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as u16)
        };
        self.preview_scroll.set(next);
    }

    /// Scroll the preview by the last known viewport height.
    pub fn scroll_preview_pages(&self, delta_pages: i16) {
        let rows = self.preview_viewport_height.get().max(1);
        let delta = delta_pages.saturating_mul(rows as i16);
        self.scroll_preview_lines(delta);
    }

    /// Ask the next render to clamp preview scrolling to the bottom.
    pub fn scroll_preview_to_bottom(&self) {
        self.preview_scroll.set(u16::MAX);
    }

    /// Select the next conflict (for ConflictReview).
    pub fn select_next_conflict(&mut self) {
        if self.conflicts.is_empty() {
            return;
        }
        self.conflict_index = (self.conflict_index + 1) % self.conflicts.len();
    }

    /// Select the previous conflict.
    pub fn select_previous_conflict(&mut self) {
        if self.conflicts.is_empty() {
            return;
        }
        self.conflict_index = if self.conflict_index == 0 {
            self.conflicts.len() - 1
        } else {
            self.conflict_index - 1
        };
    }

    /// Get the currently selected conflict, if any.
    pub fn selected_conflict(&self) -> Option<&LocalConflict> {
        self.conflicts.get(self.conflict_index)
    }

    /// Insert a character at the cursor in body_buffer.
    pub fn insert_at_cursor(&mut self, c: char) {
        if self.cursor_pos <= self.body_buffer.len() {
            self.body_buffer.insert(self.cursor_pos, c);
            self.cursor_pos += c.len_utf8();
        }
    }

    /// Delete the character before the cursor (backspace).
    pub fn backspace_at_cursor(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.body_buffer[..self.cursor_pos]
                .chars()
                .next_back()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            let start = self.cursor_pos - prev;
            self.body_buffer.replace_range(start..self.cursor_pos, "");
            self.cursor_pos = start;
        }
    }

    /// Delete the character after the cursor (delete key).
    pub fn delete_at_cursor(&mut self) {
        if self.cursor_pos < self.body_buffer.len() {
            let len = self.body_buffer[self.cursor_pos..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.body_buffer
                .replace_range(self.cursor_pos..self.cursor_pos + len, "");
        }
    }

    /// Move cursor left by one grapheme (previous char boundary).
    pub fn cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.body_buffer[..self.cursor_pos]
                .chars()
                .next_back()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.cursor_pos -= prev;
        }
    }

    /// Move cursor right by one grapheme (next char boundary).
    pub fn cursor_right(&mut self) {
        if self.cursor_pos < self.body_buffer.len() {
            let len = self.body_buffer[self.cursor_pos..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.cursor_pos += len;
        }
    }

    /// Move cursor to the start of the current logical line.
    pub fn cursor_line_start(&mut self) {
        let cursor = clamp_to_char_boundary(&self.body_buffer, self.cursor_pos);
        self.cursor_pos = line_bounds(&self.body_buffer, cursor).0;
    }

    /// Move cursor to the end of the current logical line.
    pub fn cursor_line_end(&mut self) {
        let cursor = clamp_to_char_boundary(&self.body_buffer, self.cursor_pos);
        self.cursor_pos = line_bounds(&self.body_buffer, cursor).1;
    }

    /// Move cursor to the start of the previous word.
    pub fn cursor_word_left(&mut self) {
        self.cursor_pos = previous_word_start(&self.body_buffer, self.cursor_pos);
    }

    /// Move cursor to the start of the next word.
    pub fn cursor_word_right(&mut self) {
        self.cursor_pos = next_word_start(&self.body_buffer, self.cursor_pos);
    }

    /// Delete the word before the cursor, including intervening separators.
    pub fn delete_word_before_cursor(&mut self) {
        let cursor = clamp_to_char_boundary(&self.body_buffer, self.cursor_pos);
        let start = previous_word_start(&self.body_buffer, cursor);
        if start < cursor {
            self.body_buffer.replace_range(start..cursor, "");
            self.cursor_pos = start;
        }
    }

    /// Move cursor up one line (approximate: go back one newline if possible).
    pub fn cursor_up(&mut self) {
        let before = &self.body_buffer[..self.cursor_pos];
        if let Some(newline_pos) = before.rfind('\n') {
            self.cursor_pos = newline_pos;
        } else {
            self.cursor_pos = 0;
        }
    }

    /// Move cursor down one line (approximate: go to next newline).
    pub fn cursor_down(&mut self) {
        let after = &self.body_buffer[self.cursor_pos..];
        if let Some(newline_pos) = after.find('\n') {
            self.cursor_pos += newline_pos + 1;
        } else {
            self.cursor_pos = self.body_buffer.len();
        }
    }

    /// Insert a newline at cursor position.
    pub fn insert_newline(&mut self) {
        let cursor = clamp_to_char_boundary(&self.body_buffer, self.cursor_pos);
        let (line_start, line_end) = line_bounds(&self.body_buffer, cursor);
        let line = &self.body_buffer[line_start..line_end];

        if let Some(marker) = markdown_list_marker(line)
            .filter(|marker| cursor.saturating_sub(line_start) >= marker.prefix_len)
        {
            let is_empty_item = cursor == line_end && line[marker.prefix_len..].trim().is_empty();
            if is_empty_item {
                self.body_buffer
                    .replace_range(line_start..line_end, &marker.indent);
                self.cursor_pos = line_start + marker.indent.len();
                self.insert_at_cursor('\n');
                return;
            }

            let insertion = format!("\n{}", marker.next_prefix);
            self.body_buffer.insert_str(cursor, &insertion);
            self.cursor_pos = cursor + insertion.len();
            return;
        }

        self.cursor_pos = cursor;
        self.insert_at_cursor('\n');
    }

    // -----------------------------------------------------------------------
    // Undo / redo
    //
    // Driven from the editor event handler: call `begin_edit(kind)` before a
    // mutating edit, `break_edit_group()` after cursor movement, and `undo` /
    // `redo` for the shortcuts. Snapshots are whole-buffer copies, which is
    // ample for note-sized text and keeps the model simple.
    // -----------------------------------------------------------------------

    fn editor_snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            body: self.body_buffer.clone(),
            cursor: self.cursor_pos,
        }
    }

    /// Clear undo/redo history. Call when opening a note in the editor so each
    /// editing session starts fresh.
    pub fn reset_edit_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit_kind = None;
    }

    /// Recompute the `[[` autocomplete from the text before the cursor. Active
    /// when an unclosed `[[` precedes the cursor on the same line; the query is
    /// the text after it. Clears when out of context or no titles match.
    /// `enabled` mirrors the `wikilinks` setting.
    pub fn refresh_wiki_complete(&mut self, enabled: bool) {
        if !enabled {
            self.wiki_complete = None;
            return;
        }
        let before = &self.body_buffer[..self.cursor_pos.min(self.body_buffer.len())];
        let Some(open) = before.rfind("[[") else {
            self.wiki_complete = None;
            return;
        };
        let query = &before[open + 2..];
        // Stay within one in-progress link: bail if the would-be title spans a
        // closing bracket or a newline.
        if query.contains(']') || query.contains('\n') {
            self.wiki_complete = None;
            return;
        }
        let start = open + 2;
        let query = query.to_string();

        // Fuzzy-rank distinct note titles against the partial query.
        let mut scored: Vec<(i32, &str)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for note in &self.notes {
            let title = note.title.as_str();
            if !seen.insert(title) {
                continue;
            }
            let score = if query.is_empty() {
                Some(0)
            } else {
                commands::fuzzy_score(title, &query)
            };
            if let Some(score) = score {
                scored.push((score, title));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        let matches: Vec<String> = scored.into_iter().take(8).map(|(_, t)| t.to_string()).collect();

        if matches.is_empty() {
            self.wiki_complete = None;
            return;
        }
        let selected = self
            .wiki_complete
            .as_ref()
            .map(|w| w.selected.min(matches.len() - 1))
            .unwrap_or(0);
        self.wiki_complete = Some(WikiComplete {
            start,
            query,
            matches,
            selected,
        });
    }

    /// Move the autocomplete highlight by `delta`, wrapping.
    pub fn wiki_complete_move(&mut self, delta: i32) {
        if let Some(w) = self.wiki_complete.as_mut() {
            let n = w.matches.len() as i32;
            if n > 0 {
                w.selected = (((w.selected as i32 + delta) % n + n) % n) as usize;
            }
        }
    }

    /// Accept the highlighted candidate: replace the typed query with the full
    /// `Title]]` as a single undoable edit, leaving the cursor after `]]`.
    /// Returns the inserted title, or `None` if nothing was active.
    pub fn wiki_complete_accept(&mut self) -> Option<String> {
        let w = self.wiki_complete.take()?;
        let title = w.matches.get(w.selected).cloned()?;
        let end = self.cursor_pos.min(self.body_buffer.len());
        let start = w.start.min(end);
        self.break_edit_group();
        self.begin_edit(EditKind::Insert); // snapshot for undo
        let insert = format!("{title}]]");
        self.body_buffer.replace_range(start..end, &insert);
        self.cursor_pos = start + insert.len();
        self.break_edit_group();
        Some(title)
    }

    /// Mark the start of an edit. Consecutive edits of the same kind coalesce
    /// into one undo step; a different kind (or a preceding navigation/undo)
    /// begins a new step and invalidates the redo stack.
    pub fn begin_edit(&mut self, kind: EditKind) {
        if self.last_edit_kind != Some(kind) {
            self.undo_stack.push(self.editor_snapshot());
            if self.undo_stack.len() > MAX_UNDO_DEPTH {
                self.undo_stack.remove(0);
            }
            self.redo_stack.clear();
        }
        self.last_edit_kind = Some(kind);
    }

    /// End the current edit group so the next edit starts a fresh undo step.
    pub fn break_edit_group(&mut self) {
        self.last_edit_kind = None;
    }

    /// Undo the last edit group. Returns false if there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(self.editor_snapshot());
            self.body_buffer = prev.body;
            self.cursor_pos = prev.cursor;
            self.last_edit_kind = None;
            true
        } else {
            false
        }
    }

    /// Redo the last undone edit group. Returns false if there is nothing.
    pub fn redo(&mut self) -> bool {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(self.editor_snapshot());
            self.body_buffer = next.body;
            self.cursor_pos = next.cursor;
            self.last_edit_kind = None;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MarkdownListMarker {
    indent: String,
    prefix_len: usize,
    next_prefix: String,
}

fn clamp_to_char_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn line_bounds(text: &str, cursor: usize) -> (usize, usize) {
    let cursor = clamp_to_char_boundary(text, cursor);
    let start = text[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[cursor..]
        .find('\n')
        .map(|i| cursor + i)
        .unwrap_or(text.len());
    (start, end)
}

fn previous_word_start(text: &str, cursor: usize) -> usize {
    let mut cursor = clamp_to_char_boundary(text, cursor);

    while let Some((i, ch)) = previous_char(text, cursor) {
        if is_word_char(ch) {
            break;
        }
        cursor = i;
    }

    while let Some((i, ch)) = previous_char(text, cursor) {
        if !is_word_char(ch) {
            break;
        }
        cursor = i;
    }

    cursor
}

fn next_word_start(text: &str, cursor: usize) -> usize {
    let mut cursor = clamp_to_char_boundary(text, cursor);

    while let Some((i, ch)) = next_char(text, cursor) {
        if !is_word_char(ch) {
            break;
        }
        cursor = i + ch.len_utf8();
    }

    while let Some((i, ch)) = next_char(text, cursor) {
        if is_word_char(ch) {
            break;
        }
        cursor = i + ch.len_utf8();
    }

    cursor
}

fn previous_char(text: &str, cursor: usize) -> Option<(usize, char)> {
    text[..cursor].char_indices().next_back()
}

fn next_char(text: &str, cursor: usize) -> Option<(usize, char)> {
    text[cursor..]
        .char_indices()
        .next()
        .map(|(offset, ch)| (cursor + offset, ch))
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn markdown_list_marker(line: &str) -> Option<MarkdownListMarker> {
    let indent_len = line
        .bytes()
        .take_while(|b| matches!(b, b' ' | b'\t'))
        .count();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];
    let bytes = rest.as_bytes();

    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1] == b' ' {
        let marker = bytes[0] as char;
        let after_bullet = &rest[2..];
        if is_task_marker(after_bullet) {
            return Some(MarkdownListMarker {
                indent: indent.to_string(),
                prefix_len: indent_len + 6,
                next_prefix: format!("{indent}{marker} [ ] "),
            });
        }

        return Some(MarkdownListMarker {
            indent: indent.to_string(),
            prefix_len: indent_len + 2,
            next_prefix: format!("{indent}{marker} "),
        });
    }

    let digit_len = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digit_len > 0
        && bytes.get(digit_len) == Some(&b'.')
        && bytes.get(digit_len + 1) == Some(&b' ')
    {
        let number = rest[..digit_len].parse::<u64>().unwrap_or(0);
        return Some(MarkdownListMarker {
            indent: indent.to_string(),
            prefix_len: indent_len + digit_len + 2,
            next_prefix: format!("{}{}. ", indent, number.saturating_add(1)),
        });
    }

    None
}

fn is_task_marker(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 4
        && bytes[0] == b'['
        && matches!(bytes[1], b' ' | b'x' | b'X')
        && bytes[2] == b']'
        && bytes[3] == b' '
}

/// Byte offsets of every (non-overlapping) case-insensitive occurrence of
/// `needle` in `haystack`. ASCII-case-insensitive: `to_ascii_lowercase`
/// preserves byte length, so offsets stay valid char boundaries in the original.
/// (Full Unicode case-folding would shift offsets — deferred to v2.)
pub fn find_matches(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let hay = haystack.to_ascii_lowercase();
    let need = needle.to_ascii_lowercase();
    hay.match_indices(&need).map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_open_selects_nearest_and_jump_scrolls() {
        use crate::tui::markdown::Heading;
        let mut state = AppState::new();
        *state.preview_headings.borrow_mut() = vec![
            Heading { level: 1, text: "A".into(), row: 2 },
            Heading { level: 2, text: "B".into(), row: 10 },
            Heading { level: 1, text: "C".into(), row: 20 },
        ];
        state.preview_scroll.set(12);

        // Opening selects the heading at or above the current scroll (row 10).
        state.open_outline();
        assert!(state.outline_open);
        assert_eq!(state.outline_selected, 1);

        // Movement is clamped to the heading range.
        state.outline_move(1);
        assert_eq!(state.outline_selected, 2);
        state.outline_move(5);
        assert_eq!(state.outline_selected, 2);
        state.outline_move(-9);
        assert_eq!(state.outline_selected, 0);

        // Jump scrolls so the selected heading (row 2) sits near the top.
        state.outline_selected = 2;
        state.jump_to_selected_heading();
        assert_eq!(state.preview_scroll.get(), 19);

        state.close_outline();
        assert!(!state.outline_open);
    }

    #[test]
    fn open_outline_noops_without_headings() {
        let mut state = AppState::new();
        state.open_outline();
        assert!(!state.outline_open);
    }

    #[test]
    fn toggle_fold_marks_nearest_heading_and_fold_all() {
        use crate::tui::markdown::Heading;
        let mut state = AppState::new();
        *state.preview_headings.borrow_mut() = vec![
            Heading { level: 1, text: "A".into(), row: 0 },
            Heading { level: 1, text: "B".into(), row: 10 },
        ];
        state.preview_scroll.set(12); // nearest at/above row 12 is heading 1

        state.toggle_fold_at_scroll();
        assert!(state.folded_headings.borrow().contains(&1));
        state.toggle_fold_at_scroll(); // toggles back off
        assert!(!state.folded_headings.borrow().contains(&1));

        state.toggle_fold_all();
        assert_eq!(state.folded_headings.borrow().len(), 2);
        state.toggle_fold_all(); // any folded → clear all
        assert!(state.folded_headings.borrow().is_empty());
    }

    #[test]
    fn toggle_zen_mode_flips_state() {
        let mut state = AppState::new();
        assert!(!state.zen_mode);
        state.toggle_zen_mode();
        assert!(state.zen_mode);
        state.toggle_zen_mode();
        assert!(!state.zen_mode);
    }

    #[test]
    fn refresh_code_blocks_clears_folds() {
        let mut state = AppState::new();
        state.folded_headings.borrow_mut().insert(0);
        state.preview_body = "# A\n\nbody".to_string();
        state.refresh_code_blocks();
        assert!(state.folded_headings.borrow().is_empty());
    }

    #[test]
    fn preview_search_step_wraps_around_matches() {
        let mut state = AppState::new();
        *state.preview_search_matches.borrow_mut() = vec![(0, 0, 1), (2, 0, 1), (5, 0, 1)];
        state.preview_search_idx = 2;
        state.preview_search_step(1); // wraps to the first match
        assert_eq!(state.preview_search_idx, 0);
        state.preview_search_step(-1); // wraps back to the last
        assert_eq!(state.preview_search_idx, 2);
        assert!(state.preview_search_scroll.get());
    }

    #[test]
    fn preview_search_lifecycle_open_confirm_cancel() {
        let mut state = AppState::new();
        state.view = AppView::Preview;
        state.open_preview_search();
        assert_eq!(state.view, AppView::PreviewSearch);
        state.preview_search_input('h');
        state.preview_search_input('i');
        assert_eq!(state.preview_search_query, "hi");
        state.confirm_preview_search();
        assert_eq!(state.view, AppView::Preview);
        assert_eq!(state.preview_search_query, "hi"); // kept for n/N
        state.cancel_preview_search();
        assert!(state.preview_search_query.is_empty());
    }

    #[test]
    fn palette_opens_filters_and_selects() {
        let mut state = AppState::new();
        state.open_palette();
        assert_eq!(state.view, AppView::CommandPalette);
        // Empty query lists the whole registry, first item selected.
        assert_eq!(state.palette_matches.len(), commands::commands().len());
        assert_eq!(state.palette_state.selected(), Some(0));
        assert!(state.selected_palette_command().is_some());

        // Typing narrows; "sync now" resolves to exactly the SyncNow command.
        for c in "sync now".chars() {
            state.palette_input(c);
        }
        assert_eq!(state.selected_palette_command(), Some(CommandId::SyncNow));

        // A query with no subsequence match clears the list and selection.
        state.palette_query.clear();
        for c in "zzzz".chars() {
            state.palette_input(c);
        }
        assert!(state.palette_matches.is_empty());
        assert_eq!(state.selected_palette_command(), None);
    }

    #[test]
    fn settings_form_seeds_from_settings() {
        let mut s = Settings::default();
        s.sync.enabled = true;
        s.sync.database_url = Some("postgres://localhost/jot".to_string());
        s.sync.poll_interval_seconds = 60;
        s.editor.autosave_seconds = 3;

        let form = SettingsForm::from_settings(&s);
        assert!(form.sync_enabled);
        assert_eq!(form.database_url, "postgres://localhost/jot");
        assert_eq!(form.poll_interval, "60");
        assert_eq!(form.autosave, "3");
    }

    #[test]
    fn settings_form_applies_and_validates() {
        let mut form = SettingsForm::from_settings(&Settings::default());
        form.data_dir = "/tmp/jotdata".to_string();
        form.db_path = "/tmp/jotdata/jot.db".to_string();
        form.sync_enabled = true;
        form.database_url = "postgres://localhost/jot".to_string();
        form.poll_interval = "20".to_string();
        form.autosave = "7".to_string();

        let mut s = Settings::default();
        form.apply_to(&mut s).expect("valid form should apply");
        assert!(s.sync.enabled);
        assert_eq!(
            s.sync.database_url.as_deref(),
            Some("postgres://localhost/jot")
        );
        assert_eq!(s.sync.poll_interval_seconds, 20);
        assert_eq!(s.editor.autosave_seconds, 7);

        // Enabling sync without a URL is rejected.
        form.database_url = "  ".to_string();
        assert!(form.apply_to(&mut s).is_err());

        // Non-numeric interval is rejected.
        form.database_url = "postgres://localhost/jot".to_string();
        form.poll_interval = "abc".to_string();
        assert!(form.apply_to(&mut s).is_err());
    }

    #[test]
    fn settings_form_numeric_fields_reject_non_digits() {
        let mut form = SettingsForm::default();
        form.field = 4; // poll interval
        form.input_char('3');
        form.input_char('x'); // ignored
        form.input_char('0');
        assert_eq!(form.poll_interval, "30");
    }

    #[test]
    fn settings_form_field_navigation_wraps() {
        let mut form = SettingsForm::default();
        assert_eq!(form.field, 0);
        form.prev_field();
        assert_eq!(form.field, SettingsForm::FIELD_COUNT - 1);
        form.next_field();
        assert_eq!(form.field, 0);
    }

    #[test]
    fn settings_form_round_trips_ai_fields() {
        let mut s = Settings::default();
        s.ai.enabled = true;
        s.ai.search_default = "keyword".to_string();
        s.ai.chat.base_url = "https://api.example.com/v1".to_string();
        s.ai.chat.model = "gpt-4o-mini".to_string();
        s.ai.chat.api_key_env = Some("OPENAI_KEY".to_string());
        s.ai.chat.allow_remote = true;

        let form = SettingsForm::from_settings(&s);
        assert_eq!(form.chat_base_url, "https://api.example.com/v1");
        assert_eq!(form.chat_api_key_env, "OPENAI_KEY");
        assert!(form.chat_allow_remote);

        let mut out = Settings::default();
        form.apply_to(&mut out).expect("apply");
        assert_eq!(out.ai.search_default, "keyword");
        assert_eq!(out.ai.chat.base_url, "https://api.example.com/v1");
        assert_eq!(out.ai.chat.model, "gpt-4o-mini");
        assert_eq!(out.ai.chat.api_key_env.as_deref(), Some("OPENAI_KEY"));
        assert!(out.ai.chat.allow_remote);
    }

    #[test]
    fn empty_api_key_env_becomes_none() {
        let mut form = SettingsForm::from_settings(&Settings::default());
        form.chat_api_key_env = "  ".to_string();
        let mut out = Settings::default();
        form.apply_to(&mut out).expect("apply");
        assert!(out.ai.chat.api_key_env.is_none());
    }

    #[test]
    fn toggle_or_cycle_flips_bools_and_cycles_choice() {
        let mut form = SettingsForm::from_settings(&Settings::default());
        form.field = 7; // Default search (choice)
        let before = form.search_default.clone();
        form.toggle_or_cycle();
        assert_ne!(form.search_default, before);
        assert!(matches!(
            form.search_default.as_str(),
            "keyword" | "semantic"
        ));

        form.field = 11; // Allow remote (bool)
        let was = form.chat_allow_remote;
        form.toggle_or_cycle();
        assert_eq!(form.chat_allow_remote, !was);
    }

    #[test]
    fn remote_endpoint_warning_set_and_cleared() {
        let mut form = SettingsForm::from_settings(&Settings::default());
        form.chat_base_url = "https://api.example.com/v1".to_string();
        form.field = 11;
        form.toggle_or_cycle(); // allow_remote -> true
        assert!(form.status.starts_with("⚠ Remote AI"));
        form.toggle_or_cycle(); // -> false
        assert!(form.status.is_empty());
    }

    #[test]
    fn url_is_local_detects_loopback() {
        assert!(url_is_local("http://localhost:11434/v1"));
        assert!(url_is_local("http://127.0.0.1:8080/v1"));
        assert!(!url_is_local("https://api.openai.com/v1"));
    }

    #[test]
    fn preview_scroll_helpers_saturate_and_reset() {
        let state = AppState::new();
        state.scroll_preview_lines(5);
        assert_eq!(state.preview_scroll.get(), 5);
        state.scroll_preview_lines(-10);
        assert_eq!(state.preview_scroll.get(), 0);

        state.preview_viewport_height.set(7);
        state.scroll_preview_pages(2);
        assert_eq!(state.preview_scroll.get(), 14);
        state.scroll_preview_to_bottom();
        assert_eq!(state.preview_scroll.get(), u16::MAX);
        state.reset_preview_scroll();
        assert_eq!(state.preview_scroll.get(), 0);
    }

    #[test]
    fn open_ask_resets_ask_state() {
        let mut state = AppState::new();
        state.ask_input.push_str("stale");
        state.ask_answer = Some("old answer".to_string());
        state.ask_pending = true;

        state.open_ask();

        assert_eq!(state.view, AppView::Ask);
        assert!(state.ask_input.is_empty());
        assert!(state.ask_answer.is_none());
        assert!(state.ask_citations.is_empty());
        assert!(!state.ask_pending);
    }

    fn editor_state(body: &str, cursor_pos: usize) -> AppState {
        let mut state = AppState::new();
        state.body_buffer = body.to_string();
        state.cursor_pos = cursor_pos;
        state
    }

    #[test]
    fn editor_home_end_move_to_logical_line_boundaries() {
        let body = "one\ntwo three\nfour";
        let mut state = editor_state(body, body.find("three").unwrap());

        state.cursor_line_start();
        assert_eq!(state.cursor_pos, 4);

        state.cursor_line_end();
        assert_eq!(state.cursor_pos, "one\ntwo three".len());
    }

    #[test]
    fn editor_word_motion_crosses_punctuation_and_multibyte_text() {
        let body = "alpha, beta γamma";
        let mut state = editor_state(body, 0);

        state.cursor_word_right();
        assert_eq!(state.cursor_pos, body.find("beta").unwrap());

        state.cursor_word_right();
        assert_eq!(state.cursor_pos, body.find("γamma").unwrap());

        state.cursor_word_left();
        assert_eq!(state.cursor_pos, body.find("beta").unwrap());

        state.cursor_word_left();
        assert_eq!(state.cursor_pos, 0);
    }

    #[test]
    fn editor_word_backspace_deletes_word_and_separators() {
        let body = "alpha beta,  γamma";
        let mut state = editor_state(body, body.len());

        state.delete_word_before_cursor();
        assert_eq!(state.body_buffer, "alpha beta,  ");
        assert_eq!(state.cursor_pos, "alpha beta,  ".len());

        state.delete_word_before_cursor();
        assert_eq!(state.body_buffer, "alpha ");
        assert_eq!(state.cursor_pos, "alpha ".len());
    }

    #[test]
    fn editor_enter_inserts_plain_newline_without_list_marker() {
        let mut state = editor_state("hello world", "hello".len());

        state.insert_newline();

        assert_eq!(state.body_buffer, "hello\n world");
        assert_eq!(state.cursor_pos, "hello\n".len());
    }

    #[test]
    fn editor_enter_continues_unordered_ordered_and_task_lists() {
        let mut bullet = editor_state("- item", "- item".len());
        bullet.insert_newline();
        assert_eq!(bullet.body_buffer, "- item\n- ");
        assert_eq!(bullet.cursor_pos, "- item\n- ".len());

        let mut ordered = editor_state("9. item", "9. item".len());
        ordered.insert_newline();
        assert_eq!(ordered.body_buffer, "9. item\n10. ");
        assert_eq!(ordered.cursor_pos, "9. item\n10. ".len());

        let mut task = editor_state("- [x] done", "- [x] done".len());
        task.insert_newline();
        assert_eq!(task.body_buffer, "- [x] done\n- [ ] ");
        assert_eq!(task.cursor_pos, "- [x] done\n- [ ] ".len());
    }

    #[test]
    fn editor_enter_terminates_empty_list_item() {
        let mut bullet = editor_state("- ", "- ".len());
        bullet.insert_newline();
        assert_eq!(bullet.body_buffer, "\n");
        assert_eq!(bullet.cursor_pos, 1);

        let mut nested = editor_state("  - [ ] ", "  - [ ] ".len());
        nested.insert_newline();
        assert_eq!(nested.body_buffer, "  \n");
        assert_eq!(nested.cursor_pos, "  \n".len());
    }

    #[test]
    fn editor_list_continuation_only_triggers_at_line_start_markers() {
        let mut state = editor_state("not - a list", "not - a list".len());

        state.insert_newline();

        assert_eq!(state.body_buffer, "not - a list\n");
        assert_eq!(state.cursor_pos, "not - a list\n".len());
    }

    #[test]
    fn editor_list_continuation_does_not_trigger_before_marker_prefix() {
        let mut state = editor_state("- item", 0);

        state.insert_newline();

        assert_eq!(state.body_buffer, "\n- item");
        assert_eq!(state.cursor_pos, 1);
    }

    /// Simulate typing a run of characters as one coalesced insert group.
    fn type_text(state: &mut AppState, text: &str) {
        for c in text.chars() {
            state.begin_edit(EditKind::Insert);
            state.insert_at_cursor(c);
        }
    }

    #[test]
    fn undo_and_redo_round_trip() {
        let mut state = AppState::new();
        type_text(&mut state, "hello");
        assert_eq!(state.body_buffer, "hello");

        assert!(state.undo());
        assert_eq!(state.body_buffer, "");

        assert!(state.redo());
        assert_eq!(state.body_buffer, "hello");
    }

    #[test]
    fn consecutive_inserts_coalesce_into_one_step() {
        let mut state = AppState::new();
        type_text(&mut state, "abc");
        // A single undo reverts the whole typed run.
        assert!(state.undo());
        assert_eq!(state.body_buffer, "");
        assert!(!state.undo());
    }

    #[test]
    fn insert_and_delete_are_separate_steps() {
        let mut state = AppState::new();
        type_text(&mut state, "ab");
        state.begin_edit(EditKind::Delete);
        state.backspace_at_cursor();
        assert_eq!(state.body_buffer, "a");

        assert!(state.undo());
        assert_eq!(state.body_buffer, "ab");
        assert!(state.undo());
        assert_eq!(state.body_buffer, "");
    }

    #[test]
    fn break_edit_group_separates_runs() {
        let mut state = AppState::new();
        type_text(&mut state, "ab");
        state.break_edit_group();
        type_text(&mut state, "cd");
        assert_eq!(state.body_buffer, "abcd");

        assert!(state.undo());
        assert_eq!(state.body_buffer, "ab");
        assert!(state.undo());
        assert_eq!(state.body_buffer, "");
    }

    #[test]
    fn typing_after_undo_discards_redo() {
        let mut state = AppState::new();
        type_text(&mut state, "ab");
        assert!(state.undo());
        type_text(&mut state, "x");
        assert_eq!(state.body_buffer, "x");
        assert!(!state.redo(), "redo must be cleared by a fresh edit");
    }

    #[test]
    fn undo_restores_cursor_position() {
        let mut state = AppState::new();
        type_text(&mut state, "abc");
        let cursor_before_delete = state.cursor_pos;
        state.begin_edit(EditKind::Delete);
        state.backspace_at_cursor();

        assert!(state.undo());
        assert_eq!(state.body_buffer, "abc");
        assert_eq!(state.cursor_pos, cursor_before_delete);
    }

    #[test]
    fn undo_and_redo_are_false_when_empty() {
        let mut state = AppState::new();
        assert!(!state.undo());
        assert!(!state.redo());
    }

    #[test]
    fn reset_edit_history_clears_stacks() {
        let mut state = AppState::new();
        type_text(&mut state, "abc");
        state.reset_edit_history();
        assert!(!state.undo(), "history cleared on reset");
    }

    // ── find_matches ──

    // ── code-block focus ──

    #[test]
    fn code_block_focus_cycles_and_clears_on_refresh() {
        let mut state = AppState::new();
        state.preview_body = "```sh\necho a\n```\n\n```python\nb\n```".to_string();
        state.refresh_code_blocks();
        assert_eq!(state.preview_code_blocks.len(), 2);
        assert_eq!(state.focused_code_index(), None);

        state.focus_next_code_block();
        assert_eq!(state.focused_code_index(), Some(0));
        assert!(state.preview_focus_scroll.get());

        state.focus_next_code_block();
        assert_eq!(state.focused_code_index(), Some(1));
        state.focus_next_code_block(); // wraps
        assert_eq!(state.focused_code_index(), Some(0));
        state.focus_prev_code_block(); // wraps back to last
        assert_eq!(state.focused_code_index(), Some(1));

        // A body with fewer blocks drops an out-of-range focus.
        state.preview_body = "no code here".to_string();
        state.refresh_code_blocks();
        assert!(state.preview_code_blocks.is_empty());
        assert_eq!(state.focused_code_index(), None);
    }

    #[test]
    fn code_block_focus_is_noop_without_blocks() {
        let mut state = AppState::new();
        state.preview_body = "plain text".to_string();
        state.refresh_code_blocks();
        state.focus_next_code_block();
        assert_eq!(state.focused_code_index(), None);
    }

    fn note_titled(title: &str) -> NoteSummary {
        let now = chrono::Utc::now();
        NoteSummary {
            id: Uuid::new_v4(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            tags: Vec::new(),
        }
    }

    fn state_with_titles(titles: &[&str]) -> AppState {
        let mut state = AppState::new();
        state.notes = titles.iter().map(|t| note_titled(t)).collect();
        state
    }

    #[test]
    fn wiki_complete_activates_on_open_brackets_and_filters() {
        let mut state = state_with_titles(&["Alpha", "Beta", "Alpaca"]);
        state.body_buffer = "see [[al".to_string();
        state.cursor_pos = state.body_buffer.len();
        state.refresh_wiki_complete(true);

        let wc = state.wiki_complete.as_ref().expect("active");
        assert_eq!(wc.query, "al");
        assert_eq!(wc.start, 6); // just after "[["
        // Both "Alpha" and "Alpaca" fuzzy-match "al"; "Beta" does not.
        assert!(wc.matches.contains(&"Alpha".to_string()));
        assert!(wc.matches.contains(&"Alpaca".to_string()));
        assert!(!wc.matches.contains(&"Beta".to_string()));
    }

    #[test]
    fn wiki_complete_inactive_without_open_brackets_or_when_closed() {
        let mut state = state_with_titles(&["Alpha"]);
        // No [[ before cursor.
        state.body_buffer = "plain alpha".to_string();
        state.cursor_pos = state.body_buffer.len();
        state.refresh_wiki_complete(true);
        assert!(state.wiki_complete.is_none());

        // A closing bracket ends the context.
        state.body_buffer = "[[Alpha]] done".to_string();
        state.cursor_pos = state.body_buffer.len();
        state.refresh_wiki_complete(true);
        assert!(state.wiki_complete.is_none());

        // Disabled setting clears it.
        state.body_buffer = "[[al".to_string();
        state.cursor_pos = state.body_buffer.len();
        state.refresh_wiki_complete(false);
        assert!(state.wiki_complete.is_none());
    }

    #[test]
    fn wiki_complete_accept_replaces_query_with_full_link() {
        let mut state = state_with_titles(&["Alpha", "Alpaca"]);
        state.body_buffer = "see [[al more".to_string();
        // Cursor right after "al" (before " more").
        state.cursor_pos = "see [[al".len();
        state.refresh_wiki_complete(true);
        // Force-select "Alpha" deterministically.
        let wc = state.wiki_complete.as_mut().unwrap();
        wc.selected = wc.matches.iter().position(|t| t == "Alpha").unwrap();

        let inserted = state.wiki_complete_accept();
        assert_eq!(inserted.as_deref(), Some("Alpha"));
        assert_eq!(state.body_buffer, "see [[Alpha]] more");
        assert_eq!(state.cursor_pos, "see [[Alpha]]".len());
        assert!(state.wiki_complete.is_none());

        // The whole completion is one undo step back to the pre-accept buffer.
        assert!(state.undo());
        assert_eq!(state.body_buffer, "see [[al more");
    }

    #[test]
    fn wiki_complete_move_wraps() {
        let mut state = state_with_titles(&["Alpha", "Alpaca", "Algae"]);
        state.body_buffer = "[[al".to_string();
        state.cursor_pos = state.body_buffer.len();
        state.refresh_wiki_complete(true);
        let n = state.wiki_complete.as_ref().unwrap().matches.len();
        assert!(n >= 2);
        state.wiki_complete_move(-1); // wrap to last
        assert_eq!(state.wiki_complete.as_ref().unwrap().selected, n - 1);
        state.wiki_complete_move(1); // wrap back to first
        assert_eq!(state.wiki_complete.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn focus_cycle_spans_code_blocks_then_links() {
        let mut state = AppState::new();
        state.preview_body = "```sh\necho a\n```".to_string();
        state.refresh_code_blocks();
        state.preview_links = vec![
            PreviewLink { display: "One".into(), target: None },
            PreviewLink { display: "Two".into(), target: None },
        ];
        // Order: Code(0), Link(0), Link(1), then wrap.
        state.focus_next_code_block();
        assert_eq!(state.preview_focus, Some(PreviewFocus::Code(0)));
        state.focus_next_code_block();
        assert_eq!(state.preview_focus, Some(PreviewFocus::Link(0)));
        assert_eq!(state.focused_link_index(), Some(0));
        state.focus_next_code_block();
        assert_eq!(state.preview_focus, Some(PreviewFocus::Link(1)));
        state.focus_next_code_block(); // wraps to first
        assert_eq!(state.preview_focus, Some(PreviewFocus::Code(0)));
        state.focus_prev_code_block(); // wraps back to last link
        assert_eq!(state.preview_focus, Some(PreviewFocus::Link(1)));
    }

    #[test]
    fn focus_cycle_includes_panel_links_and_nav_target() {
        let mut state = AppState::new();
        // No code blocks, one inline link, one backlink, one on-this-day.
        state.preview_links = vec![PreviewLink {
            display: "Inline".into(),
            target: Some(Uuid::new_v4()),
        }];
        let back = note_titled("Backlinker");
        let otd = note_titled("Yesterday");
        let back_id = back.id;
        let otd_id = otd.id;
        state.backlinks = vec![back];
        state.on_this_day = vec![(7, otd)];

        // Order: Link(0), Panel(0)=backlink, Panel(1)=on-this-day.
        state.focus_next_code_block();
        assert_eq!(state.preview_focus, Some(PreviewFocus::Link(0)));
        state.focus_next_code_block();
        assert_eq!(state.preview_focus, Some(PreviewFocus::Panel(0)));
        assert_eq!(state.focused_panel_index(), Some(0));
        assert_eq!(state.focused_nav_target().unwrap().1, Some(back_id));
        state.focus_next_code_block();
        assert_eq!(state.preview_focus, Some(PreviewFocus::Panel(1)));
        assert_eq!(state.focused_nav_target().unwrap().1, Some(otd_id));
        state.focus_next_code_block(); // wraps to first inline link
        assert_eq!(state.preview_focus, Some(PreviewFocus::Link(0)));
    }

    #[test]
    fn find_matches_case_insensitive() {
        assert_eq!(find_matches("Hello hello", "hello"), vec![0, 6]);
    }

    #[test]
    fn find_matches_no_match() {
        assert!(find_matches("abc", "z").is_empty());
    }

    #[test]
    fn find_matches_empty_needle() {
        assert!(find_matches("abc", "").is_empty());
    }

    #[test]
    fn find_matches_non_overlapping() {
        // "aa" at 0 and 2 — "aaa" would give [0, 1] but "aaaa" gives [0, 2]
        assert_eq!(find_matches("aaaa", "aa"), vec![0, 2]);
    }

    #[test]
    fn find_matches_offsets_valid_in_original() {
        // Each returned offset, when sliced in the original, starts with the
        // needle (case-insensitively).
        let haystack = "Foo bar fOO";
        let matches = find_matches(haystack, "foo");
        for off in &matches {
            let fragment = &haystack[*off..].chars().take(3).collect::<String>();
            assert_eq!(fragment.to_ascii_lowercase(), "foo");
        }
    }

    // ── editor search cycling ──

    #[test]
    fn editor_search_opens_on_anchor_and_cycles() {
        let mut state = AppState::new();
        state.body_buffer = "abc def abc ghi".to_string();
        state.cursor_pos = 0;

        state.open_editor_search();
        assert_eq!(state.view, AppView::EditorSearch);
        assert!(state.editor_search_query.is_empty());
        assert!(state.editor_search_matches.is_empty());

        // Type query — refreshes and jumps to first match at/after anchor (0).
        state.editor_search_query.push('a');
        state.editor_search_refresh();
        assert_eq!(state.editor_search_matches, vec![0, 8]);
        assert_eq!(state.editor_search_idx, 0);
        assert_eq!(state.cursor_pos, 0);

        // Next → wraps to 8
        state.editor_search_next();
        assert_eq!(state.editor_search_idx, 1);
        assert_eq!(state.cursor_pos, 8);

        // Next → wraps back to 0
        state.editor_search_next();
        assert_eq!(state.editor_search_idx, 0);
        assert_eq!(state.cursor_pos, 0);

        // Prev → wraps back to 8
        state.editor_search_prev();
        assert_eq!(state.editor_search_idx, 1);
        assert_eq!(state.cursor_pos, 8);
    }

    #[test]
    fn editor_search_anchor_respected() {
        // Anchor at cursor_pos when opened; nearest match at or after anchor.
        let mut state = AppState::new();
        state.body_buffer = "abc def abc ghi".to_string();
        state.cursor_pos = 6; // in the middle of "def" — before the second "abc"

        state.open_editor_search();
        state.editor_search_query.push('a');
        state.editor_search_refresh();
        // Anchor=6, so first match >=6 is the second "abc" at offset 8
        assert_eq!(state.editor_search_idx, 1);
        assert_eq!(state.cursor_pos, 8);
    }
}
