use anyhow::Result;
use chrono::Utc;
use serde_json;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::storage::remote::PostgresStorage;
use crate::storage::SqliteStorage;

/// Commands sent from the TUI to the sync worker.
///
/// `Shutdown` and `SetPollInterval` are handled by the worker loop but not yet
/// sent from the UI — reserved for graceful shutdown and runtime poll-interval
/// changes — hence the `allow`.
#[derive(Debug)]
#[allow(dead_code)]
pub enum SyncCommand {
    /// Push pending local changes and pull remote changes.
    SyncNow,
    /// Shut down the sync worker.
    Shutdown,
    /// Update the polling interval (seconds).
    SetPollInterval(u64),
}

/// Events emitted by the sync worker back to the TUI loop.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// Sync cycle started.
    Started,
    /// Sync cycle completed successfully.
    Completed { message: String },
    /// A recoverable sync error occurred.
    Failed { message: String },
}

/// Spawn a sync worker task.
///
/// The worker connects to PostgreSQL (if configured) and waits for commands.
/// Returns `None` if sync is not enabled.
pub fn spawn_sync_worker(
    postgres_storage: Option<PostgresStorage>,
    sqlite_storage: SqliteStorage,
    poll_interval_seconds: u64,
) -> Option<(mpsc::Sender<SyncCommand>, mpsc::Receiver<SyncEvent>)> {
    let pg = postgres_storage?;

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SyncCommand>(16);
    let (evt_tx, evt_rx) = mpsc::channel::<SyncEvent>(16);

    tokio::spawn(async move {
        if let Err(e) = sync_worker_task(
            &pg,
            &sqlite_storage,
            &mut cmd_rx,
            &evt_tx,
            poll_interval_seconds,
        )
        .await
        {
            tracing::error!("Sync worker exited with error: {:?}", e);
        }
    });

    Some((cmd_tx, evt_rx))
}

async fn sync_worker_task(
    pg: &PostgresStorage,
    sqlite: &SqliteStorage,
    cmd_rx: &mut mpsc::Receiver<SyncCommand>,
    evt_tx: &mpsc::Sender<SyncEvent>,
    poll_interval_seconds: u64,
) -> Result<()> {
    // Register device on startup
    let hostname = hostname().await;
    pg.register_device(&hostname).await?;
    tracing::info!("Sync worker ready — device registered as '{}'", hostname);

    let mut interval_secs = poll_interval_seconds.max(10); // minimum 10s

    // Last background error surfaced to the UI, so we don't spam the status bar
    // with the same message every poll while offline.
    let mut last_bg_error: Option<String> = None;

    // This worker is a single task and awaits each sync cycle to completion, so
    // manual and background syncs can never overlap — no in-flight flag needed.
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(SyncCommand::SyncNow) => {
                        // Manual sync forces an immediate retry of backed-off
                        // entries and always reports start + result.
                        let _ = evt_tx.send(SyncEvent::Started).await;
                        match run_sync_cycle(pg, sqlite, true).await {
                            Ok(outcome) => {
                                last_bg_error = None;
                                let _ = evt_tx.send(SyncEvent::Completed { message: outcome.message }).await;
                            }
                            Err(e) => {
                                tracing::warn!("Sync cycle failed: {:?}", e);
                                let _ = evt_tx.send(SyncEvent::Failed {
                                    message: format!("Sync failed: {}", e),
                                }).await;
                            }
                        }
                    }
                    Some(SyncCommand::SetPollInterval(secs)) => {
                        interval_secs = secs.max(10);
                        tracing::info!("Sync poll interval set to {}s", interval_secs);
                    }
                    Some(SyncCommand::Shutdown) | None => {
                        tracing::info!("Sync worker shutting down");
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {
                // Background poll with a small extra jitter delay (0–5s) so multiple
                // devices don't poll in lockstep. Must stay non-negative: casting a
                // negative i64 to u64 would wrap to a ~584-million-year sleep.
                let extra_jitter_ms = (rand::random::<f64>() * 5000.0) as u64;
                tokio::time::sleep(Duration::from_millis(extra_jitter_ms)).await;

                // Background poll: notify the UI only when something actually
                // changed (to refresh the list), or on a *new* error. Repeated
                // identical errors (e.g. while offline) are logged but not shown.
                match run_sync_cycle(pg, sqlite, false).await {
                    Ok(outcome) => {
                        last_bg_error = None;
                        if outcome.changed() {
                            let _ = evt_tx.send(SyncEvent::Completed { message: outcome.message }).await;
                        }
                    }
                    Err(e) => {
                        let message = format!("Sync failed: {}", e);
                        tracing::warn!("Background sync failed: {:?}", e);
                        if last_bg_error.as_deref() != Some(message.as_str()) {
                            last_bg_error = Some(message.clone());
                            let _ = evt_tx.send(SyncEvent::Failed { message }).await;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// The result of one sync cycle.
struct SyncOutcome {
    pushed: usize,
    pulled: usize,
    message: String,
}

impl SyncOutcome {
    /// Whether the cycle moved any data (used to suppress noisy background-poll
    /// notifications when nothing happened).
    fn changed(&self) -> bool {
        self.pushed > 0 || self.pulled > 0
    }
}

/// Run one full sync cycle: push local changes, then pull remote changes.
///
/// `force` (a user-triggered sync) clears any retry backoff so previously
/// failed entries are attempted immediately.
async fn run_sync_cycle(
    pg: &PostgresStorage,
    sqlite: &SqliteStorage,
    force: bool,
) -> Result<SyncOutcome> {
    // 1. Push pending local changes
    let pushed = push_pending_changes(pg, sqlite, force).await?;

    // 2. Pull remote changes
    let pulled = pull_remote_changes(pg, sqlite).await?;

    let mut parts: Vec<String> = Vec::new();
    if pushed > 0 {
        parts.push(format!("{} pushed", pushed));
    }
    if pulled > 0 {
        parts.push(format!("{} pulled", pulled));
    }
    let message = if parts.is_empty() {
        "Already up to date".to_string()
    } else {
        parts.join(", ")
    };

    Ok(SyncOutcome {
        pushed,
        pulled,
        message,
    })
}

/// Push all pending sync_queue entries to the remote database.
///
/// When `force` is set (manual sync) any retry backoff is cleared first so
/// previously failed entries are retried immediately. Otherwise only entries
/// whose backoff window has elapsed are attempted.
async fn push_pending_changes(
    pg: &PostgresStorage,
    sqlite: &SqliteStorage,
    force: bool,
) -> Result<usize> {
    // Compact the queue before pushing to avoid redundant work
    let _compacted = sqlite.compact_sync_queue().await?;

    if force {
        sqlite.reset_sync_backoff().await?;
    }

    let entries = sqlite.get_due_sync_queue_entries(100).await?;
    if entries.is_empty() {
        return Ok(0);
    }

    let mut pushed = 0usize;

    for entry in entries {
        let entity_id: Uuid = entry.entity_id.parse()?;

        let result = match (entry.entity_type.as_str(), entry.operation.as_str()) {
            ("note", "create") | ("note", "update") => {
                // Fetch the full note from local storage and push it
                if let Ok(Some(note)) = sqlite.get_note(entity_id).await {
                    if !note.body.is_empty() || !note.title.is_empty() {
                        pg.upsert_note(&note, None).await
                    } else {
                        pg.delete_note(entity_id).await.map(|_| true)
                    }
                } else {
                    // Note was deleted locally — push the delete
                    pg.delete_note(entity_id).await.map(|_| true)
                }
            }
            ("note", "delete") => pg.delete_note(entity_id).await.map(|_| true),
            ("tag", "create") | ("tag", "update") => {
                // Parse tag id and name from payload
                if let Some(name) = entry.payload_json.get("name").and_then(|v| v.as_str()) {
                    pg.upsert_tag(entity_id, name).await.map(|_| true)
                } else {
                    tracing::warn!("Sync queue tag entry missing 'name' in payload");
                    // Mark as attempted but skip
                    Ok(true)
                }
            }
            ("note_tag", "tag_add") => {
                if let Some(tag_id) = entry
                    .payload_json
                    .get("tag_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    // Ensure the tag row exists remotely before linking it
                    // (note_tags has a FK to tags).
                    let name = entry
                        .payload_json
                        .get("tag")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match pg.upsert_tag(tag_id, name).await {
                        Ok(_) => pg.add_note_tag(entity_id, tag_id, name).await.map(|_| true),
                        Err(e) => Err(e),
                    }
                } else {
                    tracing::warn!("Sync queue tag_add entry missing 'tag_id' in payload");
                    Ok(true)
                }
            }
            ("note_tag", "tag_remove") => {
                if let Some(tag_id) = entry
                    .payload_json
                    .get("tag_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    let name = entry
                        .payload_json
                        .get("tag")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    pg.remove_note_tag(entity_id, tag_id, name)
                        .await
                        .map(|_| true)
                } else {
                    tracing::warn!("Sync queue tag_remove entry missing 'tag_id' in payload");
                    Ok(true)
                }
            }
            _ => {
                tracing::warn!(
                    "Unknown sync queue entry type: {} / {}",
                    entry.entity_type,
                    entry.operation
                );
                Ok(true) // skip unknown entries
            }
        };

        match result {
            Ok(true) | Ok(_) => {
                sqlite.remove_sync_queue_entry(entry.id).await?;
                pushed += 1;
            }
            Err(e) => {
                let msg = e.to_string();
                let retry_after = Utc::now() + backoff_delay(entry.attempts, &msg);
                sqlite
                    .mark_sync_queue_error(entry.id, &msg, retry_after)
                    .await?;
                tracing::warn!(
                    "Failed to push sync entry {} (attempt {}, next try {}): {:?}",
                    entry.id,
                    entry.attempts + 1,
                    retry_after.to_rfc3339(),
                    e
                );
            }
        }
    }

    Ok(pushed)
}

/// Compute how long to wait before retrying a failed sync-queue entry.
///
/// Transient errors back off exponentially (5s, 10s, 20s … capped at 5 min).
/// Permanent errors (auth/config) jump straight to the cap so they don't churn,
/// while still being retried periodically in case the situation is corrected.
fn backoff_delay(attempts: i32, error_message: &str) -> chrono::Duration {
    const MAX_SECS: i64 = 300;
    if is_permanent_error(error_message) {
        return chrono::Duration::seconds(MAX_SECS);
    }
    let exp = attempts.clamp(0, 6) as u32; // cap shift so it can't overflow
    let secs = (5_i64.saturating_mul(1_i64 << exp)).min(MAX_SECS);
    chrono::Duration::seconds(secs)
}

/// Heuristic: does this error look like a permanent auth/config problem (which
/// retrying won't fix) rather than a transient network blip?
fn is_permanent_error(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("authentication")
        || m.contains("password")
        || m.contains("does not exist")
        || m.contains("permission denied")
        || m.contains("no such host")
        || m.contains("failed to parse")
        || m.contains("invalid")
}

/// Pull remote changes and apply them locally, detecting conflicts.
async fn pull_remote_changes(pg: &PostgresStorage, sqlite: &SqliteStorage) -> Result<usize> {
    // Compact before pulling as well (clean up any stale entries)
    let _compacted = sqlite.compact_sync_queue().await?;

    let last_event_id = pg.get_last_event_id().await?;
    let events = pg.fetch_events_after(last_event_id, 100).await?;
    if events.is_empty() {
        return Ok(0);
    }

    let mut max_id = last_event_id;
    // Count only changes from *other* devices that we actually applied, so the
    // caller can tell whether a sync did anything meaningful (own echoed events
    // don't count).
    let mut applied = 0usize;

    for event in &events {
        if event.device_id == pg.device_id() {
            // Skip events we created
            max_id = max_id.max(event.id);
            continue;
        }
        applied += 1;

        match (event.entity_type.as_str(), event.operation.as_str()) {
            ("note", "create") | ("note", "update") => {
                if let Ok(Some(remote)) = pg.fetch_note(event.entity_id).await {
                    // Check whether we have a local version of this note
                    if let Ok(Some(local)) = sqlite.get_note(remote.id).await {
                        // Check if local has pending unsynced changes
                        let has_pending = has_pending_changes(sqlite, remote.id).await?;

                        if has_pending && local.content_hash != remote.content_hash {
                            // Both sides changed — record conflict
                            let local_payload = serde_json::json!({
                                "title": local.title,
                                "body": local.body,
                                "content_hash": local.content_hash,
                            });
                            let remote_payload = serde_json::json!({
                                "title": remote.title,
                                "body": remote.body,
                                "content_hash": remote.content_hash,
                            });

                            if let Err(e) = sqlite
                                .create_conflict(
                                    remote.id,
                                    local_payload,
                                    remote_payload,
                                    local.remote_version,
                                )
                                .await
                            {
                                tracing::warn!("Failed to record conflict: {:?}", e);
                            } else {
                                tracing::info!(
                                    "Conflict recorded for note {} (version {})",
                                    remote.id,
                                    local.remote_version,
                                );
                            }

                            max_id = max_id.max(event.id);
                            continue;
                        }

                        // Local is clean → safe to overwrite
                        if local.content_hash == remote.content_hash {
                            // Same content — skip
                            max_id = max_id.max(event.id);
                            continue;
                        }
                    }

                    // Apply remote version (new note or non-conflicting update),
                    // preserving the remote timestamps for consistent ordering.
                    if let Err(e) = sqlite
                        .upsert_note_from_remote(
                            remote.id,
                            &remote.title,
                            &remote.body,
                            remote.created_at,
                            remote.updated_at,
                        )
                        .await
                    {
                        tracing::warn!("Failed to apply remote note {}: {:?}", remote.id, e);
                    }
                }
            }
            ("note", "delete") => {
                if let Err(e) = sqlite.soft_delete_note(event.entity_id).await {
                    tracing::warn!(
                        "Failed to apply remote deletion {}: {:?}",
                        event.entity_id,
                        e
                    );
                }
            }
            ("note_tag", "tag_add") => {
                let name = event
                    .payload_json
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !name.is_empty() {
                    if let Err(e) = sqlite.apply_remote_tag_add(event.entity_id, name).await {
                        tracing::warn!(
                            "Failed to apply remote tag_add ({} on {}): {:?}",
                            name,
                            event.entity_id,
                            e
                        );
                    }
                }
            }
            ("note_tag", "tag_remove") => {
                let name = event
                    .payload_json
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !name.is_empty() {
                    if let Err(e) = sqlite.apply_remote_tag_remove(event.entity_id, name).await {
                        tracing::warn!(
                            "Failed to apply remote tag_remove ({} on {}): {:?}",
                            name,
                            event.entity_id,
                            e
                        );
                    }
                }
            }
            _ => {
                // Skip unknown event types (e.g. standalone "tag" upserts — the
                // tag is created locally when its note_tag link is applied).
            }
        }

        max_id = max_id.max(event.id);
    }

    pg.update_cursor(max_id).await?;
    Ok(applied)
}

/// Check whether a note has pending (non-delete) sync queue entries.
async fn has_pending_changes(sqlite: &SqliteStorage, note_id: Uuid) -> Result<bool> {
    let entries = sqlite.get_sync_queue_entries(1000).await?;
    let id_str = note_id.to_string();
    Ok(entries
        .iter()
        .any(|e| e.entity_id == id_str && e.operation != "delete"))
}

/// Get the system hostname for device registration.
async fn hostname() -> String {
    tokio::task::spawn_blocking(|| {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("HOSTNAME")
                    .ok()
                    .or_else(|| std::env::var("HOST").ok())
            })
            .unwrap_or_else(|| "unknown".to_string())
    })
    .await
    .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;
    use crate::models::note::{CreateNoteInput, UpdateNoteInput};

    /// End-to-end multi-device sync verification against a live PostgreSQL.
    ///
    /// Ignored by default because it requires a real database. Run with:
    ///   JOT_TEST_DATABASE_URL=postgres://jot:jot@localhost:55432/jot \
    ///     cargo test --bin jot-down -- --ignored --nocapture full_sync_roundtrip
    #[tokio::test]
    #[ignore]
    async fn full_sync_roundtrip() {
        let url = std::env::var("JOT_TEST_DATABASE_URL")
            .expect("set JOT_TEST_DATABASE_URL to run this test");

        // Two devices sharing one remote database.
        let dev_a = Uuid::new_v4();
        let dev_b = Uuid::new_v4();
        let pg_a = PostgresStorage::connect(&url, dev_a).await.expect("pg A");
        let pg_b = PostgresStorage::connect(&url, dev_b).await.expect("pg B");
        pg_a.register_device("device-a").await.expect("register A");
        pg_b.register_device("device-b").await.expect("register B");

        // Each device has its own local SQLite cache.
        let path_a = std::env::temp_dir().join(format!("jot_a_{}.db", Uuid::new_v4()));
        let path_b = std::env::temp_dir().join(format!("jot_b_{}.db", Uuid::new_v4()));
        let sq_a = SqliteStorage::connect(&path_a).await.expect("sqlite A");
        let sq_b = SqliteStorage::connect(&path_b).await.expect("sqlite B");

        // ── 1. CREATE on A propagates to B ──────────────────────────────
        let note = sq_a
            .create_note(CreateNoteInput {
                title: "Shared".into(),
                body: "created on A".into(),
            })
            .await
            .expect("create");
        let push_msg = run_sync_cycle(&pg_a, &sq_a, true).await.expect("A push");
        eprintln!("[create] A sync: {}", push_msg.message);

        let pull_msg = run_sync_cycle(&pg_b, &sq_b, true).await.expect("B pull");
        eprintln!("[create] B sync: {}", pull_msg.message);

        let on_b = sq_b
            .get_note(note.id)
            .await
            .expect("B get")
            .expect("note on B");
        assert_eq!(on_b.title, "Shared");
        assert_eq!(on_b.body, "created on A");
        eprintln!("[create] OK — note replicated A -> B");

        // ── 2. UPDATE on A propagates to B ──────────────────────────────
        sq_a.update_note(UpdateNoteInput {
            id: note.id,
            title: "Shared".into(),
            body: "edited on A".into(),
        })
        .await
        .expect("update");
        run_sync_cycle(&pg_a, &sq_a, true)
            .await
            .expect("A push update");
        run_sync_cycle(&pg_b, &sq_b, true)
            .await
            .expect("B pull update");

        let on_b = sq_b.get_note(note.id).await.expect("B get").expect("note");
        assert_eq!(on_b.body, "edited on A");
        eprintln!("[update] OK — edit replicated A -> B");

        // ── 3. TAG add on A reaches the remote table AND propagates to B ─
        let tag = sq_a
            .add_tag_to_note(note.id, "important")
            .await
            .expect("tag");
        run_sync_cycle(&pg_a, &sq_a, true)
            .await
            .expect("A push tag");
        let tag_links: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM note_tags WHERE note_id = $1 AND tag_id = $2",
        )
        .bind(note.id)
        .bind(tag.id)
        .fetch_one(pg_a.pool_for_tests())
        .await
        .expect("count note_tags");
        assert_eq!(tag_links, 1, "tag link should exist remotely");

        run_sync_cycle(&pg_b, &sq_b, true)
            .await
            .expect("B pull tag");
        let on_b = sq_b.get_note(note.id).await.expect("B get").expect("note");
        assert!(
            on_b.tags.contains(&"important".to_string()),
            "tag should propagate A -> B (got {:?})",
            on_b.tags
        );
        eprintln!("[tag] OK — tag pushed to remote and pulled onto B");

        // ── 3b. TAG remove on A propagates to B ─────────────────────────
        sq_a.remove_tag_by_name(note.id, "important")
            .await
            .expect("untag");
        run_sync_cycle(&pg_a, &sq_a, true)
            .await
            .expect("A push untag");
        run_sync_cycle(&pg_b, &sq_b, true)
            .await
            .expect("B pull untag");
        let on_b = sq_b.get_note(note.id).await.expect("B get").expect("note");
        assert!(
            !on_b.tags.contains(&"important".to_string()),
            "tag removal should propagate A -> B (got {:?})",
            on_b.tags
        );
        eprintln!("[tag] OK — tag removal propagated A -> B");

        // ── 4. CONFLICT: both sides edit the same note ──────────────────
        // A edits and pushes; B edits locally (pending) then pulls.
        sq_a.update_note(UpdateNoteInput {
            id: note.id,
            title: "Shared".into(),
            body: "A wins?".into(),
        })
        .await
        .expect("A edit");
        run_sync_cycle(&pg_a, &sq_a, true)
            .await
            .expect("A push conflict edit");

        sq_b.update_note(UpdateNoteInput {
            id: note.id,
            title: "Shared".into(),
            body: "B wins?".into(),
        })
        .await
        .expect("B edit");
        // Pull only (not a full cycle) so B's pending change is still queued.
        let pulled = pull_remote_changes(&pg_b, &sq_b)
            .await
            .expect("B pull conflict");
        eprintln!("[conflict] B pulled {} event(s)", pulled);

        let conflicts = sq_b.list_conflicts().await.expect("list conflicts");
        assert!(
            !conflicts.is_empty(),
            "B should have recorded a conflict for the divergent edit"
        );
        eprintln!(
            "[conflict] OK — {} conflict(s) detected on B",
            conflicts.len()
        );

        // ── 5. DELETE on A propagates to B ──────────────────────────────
        // Resolve B's conflict first (keep local) so the pending edit clears,
        // then let the delete flow through.
        let conflict_id = conflicts[0].id;
        sq_b.resolve_conflict(conflict_id, "keep-local")
            .await
            .expect("resolve");
        // Drain B's pending queue so the subsequent delete isn't masked.
        run_sync_cycle(&pg_b, &sq_b, true).await.expect("B drain");

        sq_a.soft_delete_note(note.id).await.expect("A delete");
        run_sync_cycle(&pg_a, &sq_a, true)
            .await
            .expect("A push delete");
        run_sync_cycle(&pg_b, &sq_b, true)
            .await
            .expect("B pull delete");

        let after_delete = sq_b.get_note(note.id).await.expect("B get after delete");
        assert!(after_delete.is_none(), "note should be soft-deleted on B");
        eprintln!("[delete] OK — deletion replicated A -> B");

        // Cleanup local DB files.
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
        eprintln!("\nFull sync round-trip verified.");
    }
}
