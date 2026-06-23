#[cfg(feature = "ai")]
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::models::note::{CreateNoteInput, Note, NoteSummary, UpdateNoteInput};
use crate::models::tag::Tag;

/// Register the statically-linked sqlite-vec extension as a SQLite
/// auto-extension, so every connection opened afterward (including sqlx's) gets
/// the `vec0` virtual-table module. No filesystem load and no system install —
/// the extension is compiled into the binary via the `sqlite-vec` crate.
#[cfg(feature = "ai")]
fn register_sqlite_vec() {
    use std::sync::Once;
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        // SAFETY: `sqlite3_auto_extension` installs a process-global entry point
        // invoked on every new sqlite connection. `sqlite_vec::sqlite3_vec_init`
        // is the extension's C init function; the transmute reshapes its pointer
        // to the entry-point type `sqlite3_auto_extension` expects (inferred from
        // the call). Registered once before any pool is opened, and
        // `libsqlite3-sys` is pinned to sqlx's version so it shares the same
        // sqlite as sqlx's connections.
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

#[derive(Debug, Clone, Copy)]
struct Migration {
    sql: &'static str,
    requires_ai: bool,
    requires_sqlite_vec: bool,
}

impl Migration {
    const fn base(sql: &'static str) -> Self {
        Self {
            sql,
            requires_ai: false,
            requires_sqlite_vec: false,
        }
    }

    #[cfg(feature = "ai")]
    const fn ai(sql: &'static str) -> Self {
        Self {
            sql,
            requires_ai: true,
            requires_sqlite_vec: false,
        }
    }

    #[cfg(feature = "ai")]
    const fn sqlite_vec(sql: &'static str) -> Self {
        Self {
            sql,
            requires_ai: true,
            requires_sqlite_vec: true,
        }
    }
}

/// Runtime state of Jot's local AI SQLite support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiSqliteStatus {
    /// AI storage support was disabled by runtime config.
    DisabledByConfig,
    /// AI storage support was compiled out of this build. Reserved: the
    /// non-`ai` build reports status differently today, but this keeps the
    /// state space complete.
    #[allow(dead_code)]
    DisabledByBuild,
    /// sqlite-vec loaded and the vec0 virtual table module was probed.
    /// Only constructed in `ai` builds.
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    Ready { extension: String },
    /// AI bookkeeping tables can exist, but semantic/vector features are off.
    /// Only constructed in `ai` builds.
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    VectorExtensionUnavailable { error: String },
}

impl AiSqliteStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    fn ai_migrations_enabled(&self) -> bool {
        !matches!(self, Self::DisabledByConfig | Self::DisabledByBuild)
    }
}

/// Resolution strategy for a sync conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Keep the local version (discard remote).
    KeepLocal,
    /// Keep the remote version (overwrite local).
    KeepRemote,
    /// Save both versions by creating a new note for the remote copy.
    SaveBoth,
}

impl ConflictResolution {
    /// Return the string value stored in the database.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            ConflictResolution::KeepLocal => "keep-local",
            ConflictResolution::KeepRemote => "keep-remote",
            ConflictResolution::SaveBoth => "save-both",
        }
    }
}

/// A pending sync queue entry. Full row mapping; `last_error` is persisted for
/// diagnostics but not surfaced in the UI yet.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SyncQueueEntry {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: String,
    pub operation: String,
    pub payload_json: serde_json::Value,
    pub attempts: i32,
    pub last_error: Option<String>,
}

/// A conflict between local and remote versions of a note. Full row mapping;
/// `resolved_at`/`resolution` are populated on resolved rows, which the current
/// UI doesn't list yet.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LocalConflict {
    pub id: Uuid,
    pub note_id: Uuid,
    pub local_payload: serde_json::Value,
    pub remote_payload: serde_json::Value,
    pub base_version: i32,
    pub detected_at: chrono::DateTime<chrono::Utc>,
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
    pub resolution: Option<String>,
}

/// A queued embedding job: a note that needs (re)embedding, with its text.
#[cfg(feature = "ai")]
#[derive(Debug, Clone)]
pub struct EmbeddingJob {
    pub note_id: Uuid,
    pub content_hash: String,
    pub text: String,
    /// How many times embedding this note has already failed. Drives the
    /// worker's retry backoff.
    pub attempts: i32,
}

/// The local SQLite storage backend.
#[derive(Debug, Clone)]
pub struct SqliteStorage {
    pool: SqlitePool,
    ai_status: AiSqliteStatus,
}

impl SqliteStorage {
    /// Create a new SQLite storage, creating the database and running migrations.
    /// Convenience constructor that infers AI support from the build; the app
    /// uses `connect_with_ai` directly, so this is currently exercised only by
    /// tests.
    #[allow(dead_code)]
    pub async fn connect(db_path: &Path) -> Result<Self> {
        Self::connect_with_ai(db_path, cfg!(feature = "ai")).await
    }

    /// Create SQLite storage with explicit runtime control of AI storage support.
    pub async fn connect_with_ai(db_path: &Path, ai_enabled: bool) -> Result<Self> {
        let options = Self::connect_options(db_path);
        let (pool, ai_status) = Self::connect_pool(options, ai_enabled).await?;

        let storage = Self { pool, ai_status };
        storage.run_migrations().await?;

        Ok(storage)
    }

    /// Return whether sqlite-vec backed AI storage is available.
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    pub fn ai_available(&self) -> bool {
        self.ai_status.is_ready()
    }

    /// Return the SQLite AI support status detected at startup. Used by tests
    /// and reserved for richer diagnostics; the doctor uses `ai_status_reason`.
    #[allow(dead_code)]
    pub fn ai_sqlite_status(&self) -> &AiSqliteStatus {
        &self.ai_status
    }

    /// Human-readable reason AI indexing is or isn't available (for status/logs).
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    pub fn ai_status_reason(&self) -> String {
        match &self.ai_status {
            AiSqliteStatus::DisabledByConfig => "AI disabled in settings".to_string(),
            AiSqliteStatus::DisabledByBuild => "AI support not compiled in".to_string(),
            AiSqliteStatus::Ready { .. } => "ready".to_string(),
            AiSqliteStatus::VectorExtensionUnavailable { error } => {
                format!("vector index unavailable: {error}")
            }
        }
    }

    fn connect_options(db_path: &Path) -> SqliteConnectOptions {
        // Set pragmas on the connect options (not via a one-off query on the
        // pool) so every connection the pool opens enforces them. Combined with
        // the per-connection sqlite-vec extension load, this keeps connections
        // consistent even if `max_connections` is ever raised above one.
        SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
    }

    async fn connect_pool(
        options: SqliteConnectOptions,
        ai_enabled: bool,
    ) -> Result<(SqlitePool, AiSqliteStatus)> {
        if !ai_enabled {
            let pool = Self::connect_pool_with_options(options).await?;
            return Ok((pool, AiSqliteStatus::DisabledByConfig));
        }

        #[cfg(feature = "ai")]
        {
            Self::connect_pool_with_sqlite_vec(options).await
        }

        #[cfg(not(feature = "ai"))]
        {
            let pool = Self::connect_pool_with_options(options).await?;
            Ok((pool, AiSqliteStatus::DisabledByBuild))
        }
    }

    #[cfg(feature = "ai")]
    async fn connect_pool_with_sqlite_vec(
        options: SqliteConnectOptions,
    ) -> Result<(SqlitePool, AiSqliteStatus)> {
        register_sqlite_vec();

        let pool = Self::connect_pool_with_options(options).await?;
        match Self::probe_sqlite_vec(&pool).await {
            Ok(()) => {
                tracing::info!("sqlite-vec ready (statically linked)");
                Ok((
                    pool,
                    AiSqliteStatus::Ready {
                        extension: "sqlite-vec (bundled)".to_string(),
                    },
                ))
            }
            Err(error) => {
                // Shouldn't happen with the bundled extension, but degrade
                // gracefully (bookkeeping works; vector search is disabled).
                tracing::warn!(
                    "sqlite-vec probe failed; semantic AI storage disabled: {:#}",
                    error
                );
                Ok((
                    pool,
                    AiSqliteStatus::VectorExtensionUnavailable {
                        error: format!("{error:#}"),
                    },
                ))
            }
        }
    }

    async fn connect_pool_with_options(options: SqliteConnectOptions) -> Result<SqlitePool> {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .context("Failed to connect to SQLite database")
    }

    #[cfg(feature = "ai")]
    async fn probe_sqlite_vec(pool: &SqlitePool) -> Result<()> {
        let _ = sqlx::query("DROP TABLE IF EXISTS temp.__jot_vec_probe")
            .execute(pool)
            .await;

        sqlx::query(
            r#"
            CREATE VIRTUAL TABLE temp.__jot_vec_probe USING vec0(
                note_id TEXT PRIMARY KEY,
                embedding FLOAT[384]
            )
            "#,
        )
        .execute(pool)
        .await
        .context("sqlite-vec vec0 module is not usable")?;

        sqlx::query("DROP TABLE temp.__jot_vec_probe")
            .execute(pool)
            .await
            .context("failed to drop sqlite-vec probe table")?;

        Ok(())
    }

    /// Run embedded SQLite migrations in order.
    ///
    /// Each migration is tracked in `schema_migrations` so it runs exactly once.
    /// This matters because some migrations use non-idempotent statements such as
    /// `ALTER TABLE ... ADD COLUMN`, which would error if re-executed on startup.
    async fn run_migrations(&self) -> Result<()> {
        let migrations = self.migrations();

        // Ensure the migration-tracking table exists.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create schema_migrations table")?;

        let mut applied_count = 0usize;
        let mut skipped_count = 0usize;
        for (i, migration) in migrations.iter().enumerate() {
            let version = (i + 1) as i64;

            let already_applied: Option<i64> =
                sqlx::query_scalar("SELECT version FROM schema_migrations WHERE version = ?")
                    .bind(version)
                    .fetch_optional(&self.pool)
                    .await?;

            if already_applied.is_some() {
                continue;
            }

            if migration.requires_ai && !self.ai_status.ai_migrations_enabled() {
                skipped_count += 1;
                continue;
            }

            if migration.requires_sqlite_vec && !self.ai_status.is_ready() {
                if self.migration_already_present(version).await? {
                    self.record_migration(version).await?;
                } else {
                    tracing::warn!(
                        "Skipping SQLite migration #{} because sqlite-vec is unavailable",
                        version
                    );
                    skipped_count += 1;
                }
                continue;
            }

            // Databases created before migration tracking existed have no rows
            // in schema_migrations even though their schema is already migrated.
            // Detect that case so we don't re-run non-idempotent statements (an
            // `ALTER TABLE ... ADD COLUMN` would fail with "duplicate column").
            if self.migration_already_present(version).await? {
                self.record_migration(version).await?;
                continue;
            }

            sqlx::query(migration.sql)
                .execute(&self.pool)
                .await
                .with_context(|| format!("Failed to run migration #{}", version))?;

            self.record_migration(version).await?;
            applied_count += 1;
        }

        tracing::info!(
            "SQLite migrations up to date ({} applied this run, {} skipped, {} total)",
            applied_count,
            skipped_count,
            migrations.len()
        );
        Ok(())
    }

    fn migrations(&self) -> Vec<Migration> {
        let mut migrations = vec![
            Migration::base(include_str!("migrations/20250615000001_initial.sql")),
            Migration::base(include_str!("migrations/20250615000002_sync_queue.sql")),
            Migration::base(include_str!("migrations/20250615000003_conflicts.sql")),
            Migration::base(include_str!("migrations/20250615000004_content_hash.sql")),
            Migration::base(include_str!("migrations/20250615000005_sync_backoff.sql")),
        ];

        #[cfg(feature = "ai")]
        {
            migrations.push(Migration::ai(include_str!(
                "migrations/20250615000006_ai_foundation.sql"
            )));
            migrations.push(Migration::sqlite_vec(include_str!(
                "migrations/20250615000007_ai_vec_notes.sql"
            )));
            migrations.push(Migration::ai(include_str!(
                "migrations/20250615000008_embedding_backoff.sql"
            )));
        }

        // Base migrations added after the AI block so their version numbers
        // trail the AI ones, keeping existing AI databases' recorded versions
        // (1–8) pointing at the same SQL. See `first_extra_base_version`.
        migrations.push(Migration::base(include_str!(
            "migrations/20250619000009_daily_date.sql"
        )));
        migrations.push(Migration::base(include_str!(
            "migrations/20250619000010_wikilinks.sql"
        )));

        migrations
    }

    /// Version number of the first base migration appended after the (optional)
    /// AI block — 9 in an AI build (5 base + 3 AI), 6 otherwise.
    const fn first_extra_base_version() -> i64 {
        #[cfg(feature = "ai")]
        {
            9
        }
        #[cfg(not(feature = "ai"))]
        {
            6
        }
    }

    /// Record a migration version as applied.
    async fn record_migration(&self, version: i64) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?, ?)")
            .bind(version)
            .bind(Utc::now().to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Detect whether the effects of a migration are already present in the
    /// schema (used to backfill tracking for pre-tracking databases).
    ///
    /// Only the migrations with non-idempotent statements need detection;
    /// idempotent ones (`CREATE TABLE IF NOT EXISTS`, etc.) are safe to re-run.
    async fn migration_already_present(&self, version: i64) -> Result<bool> {
        match version {
            // 0003 adds notes.remote_version via ALTER TABLE.
            3 => self.column_exists("notes", "remote_version").await,
            // 0004 adds notes.content_hash via ALTER TABLE.
            4 => self.column_exists("notes", "content_hash").await,
            // 0005 adds sync_queue.next_attempt_at via ALTER TABLE.
            5 => self.column_exists("sync_queue", "next_attempt_at").await,
            // 0006 creates AI bookkeeping tables.
            6 => Ok(self.table_exists("note_embeddings").await?
                && self.table_exists("embedding_queue").await?),
            // 0007 creates the sqlite-vec virtual table.
            7 => self.table_exists("vec_notes").await,
            // 0008 adds embedding_queue.next_attempt_at via ALTER TABLE.
            8 => self.column_exists("embedding_queue", "next_attempt_at").await,
            // 0009 adds notes.daily_date via ALTER TABLE.
            v if v == Self::first_extra_base_version() => {
                self.column_exists("notes", "daily_date").await
            }
            // 0010 creates the note_links index table.
            v if v == Self::first_extra_base_version() + 1 => {
                self.table_exists("note_links").await
            }
            _ => Ok(false),
        }
    }

    /// Return true if `table` has a column named `column`.
    async fn column_exists(&self, table: &str, column: &str) -> Result<bool> {
        // PRAGMA does not accept bound parameters; the table name is an internal
        // constant, so there is no injection surface here. Returns no rows (not
        // an error) when the table does not exist.
        let rows = sqlx::query(&format!("PRAGMA table_info({})", table))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().any(|r| r.get::<String, _>("name") == column))
    }

    /// Return true if the SQLite schema has a table or virtual table named `table`.
    async fn table_exists(&self, table: &str) -> Result<bool> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(&self.pool)
        .await?;

        Ok(count > 0)
    }

    /// Enqueue a sync operation for a mutation.
    /// This creates a row in `sync_queue` that the future sync worker will drain.
    async fn enqueue_sync(
        &self,
        entity_type: &str,
        entity_id: &str,
        operation: &str,
        payload_json: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO sync_queue (entity_type, entity_id, operation, payload_json, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(entity_type)
        .bind(entity_id)
        .bind(operation)
        .bind(payload_json)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Enqueue a "create" sync operation for a note.
    async fn enqueue_note_create(&self, note: &Note) -> Result<()> {
        let payload = serde_json::json!({
            "title": note.title,
            "body": note.body,
            "created_at": note.created_at.to_rfc3339(),
        });
        self.enqueue_sync("note", &note.id.to_string(), "create", &payload.to_string())
            .await
    }

    /// Enqueue an "update" sync operation for a note.
    async fn enqueue_note_update(&self, note_id: Uuid, input: &UpdateNoteInput) -> Result<()> {
        let payload = serde_json::json!({
            "title": input.title,
            "body": input.body,
        });
        self.enqueue_sync("note", &note_id.to_string(), "update", &payload.to_string())
            .await
    }

    /// Enqueue a "delete" sync operation for a note.
    async fn enqueue_note_delete(&self, note_id: Uuid) -> Result<()> {
        self.enqueue_sync("note", &note_id.to_string(), "delete", "{}")
            .await
    }

    /// Enqueue a "tag_add" sync operation.
    async fn enqueue_tag_add(&self, note_id: Uuid, tag_id: Uuid, tag_name: &str) -> Result<()> {
        let payload = serde_json::json!({ "tag_id": tag_id.to_string(), "tag": tag_name });
        self.enqueue_sync(
            "note_tag",
            &note_id.to_string(),
            "tag_add",
            &payload.to_string(),
        )
        .await
    }

    /// Enqueue a "tag_remove" sync operation.
    async fn enqueue_tag_remove(&self, note_id: Uuid, tag_id: Uuid, tag_name: &str) -> Result<()> {
        let payload = serde_json::json!({ "tag_id": tag_id.to_string(), "tag": tag_name });
        self.enqueue_sync(
            "note_tag",
            &note_id.to_string(),
            "tag_remove",
            &payload.to_string(),
        )
        .await
    }

    /// Return true when local AI bookkeeping tables are present and should be
    /// maintained. Vector search can be unavailable while the queue still
    /// exists, so this is broader than `ai_available()`.
    fn embedding_queue_enabled(&self) -> bool {
        cfg!(feature = "ai") && self.ai_status.ai_migrations_enabled()
    }

    /// Enqueue an embedding job if the current note hash has not already been
    /// embedded. One pending row is kept per note; later edits replace older
    /// queued hashes and reset retry state.
    async fn enqueue_embedding_if_stale(&self, note_id: Uuid, content_hash: &str) -> Result<()> {
        if !self.embedding_queue_enabled() {
            return Ok(());
        }

        let note_id = note_id.to_string();
        let embedded_hash: Option<String> =
            sqlx::query_scalar("SELECT content_hash FROM note_embeddings WHERE note_id = ?")
                .bind(&note_id)
                .fetch_optional(&self.pool)
                .await?;

        if embedded_hash.as_deref() == Some(content_hash) {
            sqlx::query("DELETE FROM embedding_queue WHERE note_id = ?")
                .bind(&note_id)
                .execute(&self.pool)
                .await?;
            return Ok(());
        }

        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO embedding_queue (note_id, content_hash, enqueued_at)
            VALUES (?, ?, ?)
            ON CONFLICT(note_id) DO UPDATE SET
                content_hash = excluded.content_hash,
                enqueued_at = excluded.enqueued_at,
                attempts = 0,
                last_error = NULL,
                next_attempt_at = NULL
            "#,
        )
        .bind(&note_id)
        .bind(content_hash)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Remove local-only AI derived state for a note that is no longer active.
    async fn remove_embedding_state(&self, note_id: Uuid) -> Result<()> {
        if !self.embedding_queue_enabled() {
            return Ok(());
        }

        let note_id = note_id.to_string();
        sqlx::query("DELETE FROM embedding_queue WHERE note_id = ?")
            .bind(&note_id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM note_embeddings WHERE note_id = ?")
            .bind(&note_id)
            .execute(&self.pool)
            .await?;

        if self.ai_status.is_ready() {
            sqlx::query("DELETE FROM vec_notes WHERE note_id = ?")
                .bind(&note_id)
                .execute(&self.pool)
                .await?;
        }

        Ok(())
    }

    /// Fetch a batch of queued embedding jobs joined with live note text,
    /// oldest first. Skips soft-deleted notes. Empty when AI storage is off.
    #[cfg(feature = "ai")]
    pub async fn fetch_embedding_batch(&self, limit: i64) -> Result<Vec<EmbeddingJob>> {
        if !self.embedding_queue_enabled() {
            return Ok(Vec::new());
        }

        // Skip jobs whose backoff window hasn't elapsed yet (next_attempt_at in
        // the future). NULL means "due now".
        let now = Utc::now().to_rfc3339();
        let rows = sqlx::query(
            r#"
            SELECT q.note_id AS note_id, q.content_hash AS content_hash,
                   q.attempts AS attempts, n.title AS title, n.body AS body
            FROM embedding_queue q
            JOIN notes n ON n.id = q.note_id
            WHERE n.deleted_at IS NULL
              AND (q.next_attempt_at IS NULL OR q.next_attempt_at <= ?)
            ORDER BY q.enqueued_at ASC, q.id ASC
            LIMIT ?
            "#,
        )
        .bind(&now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let jobs = rows
            .into_iter()
            .map(|row| {
                let note_id: String = row.get("note_id");
                let title: String = row.get("title");
                let body: String = row.get("body");
                EmbeddingJob {
                    note_id: Uuid::parse_str(&note_id).unwrap_or_default(),
                    content_hash: row.get("content_hash"),
                    text: format!("{title}\n\n{body}"),
                    attempts: row.get("attempts"),
                }
            })
            .collect();

        Ok(jobs)
    }

    /// Store an embedding vector + bookkeeping and clear the queue row — but
    /// only the queue row whose hash still matches what we embedded, so a newer
    /// edit that re-queued while we were working is preserved. The vector write
    /// and the dequeue happen in one transaction.
    #[cfg(feature = "ai")]
    pub async fn store_note_embedding(
        &self,
        note_id: Uuid,
        model_id: &str,
        dimensions: usize,
        content_hash: &str,
        vector: &[f32],
    ) -> Result<()> {
        if !self.embedding_queue_enabled() {
            return Ok(());
        }

        let note_id = note_id.to_string();
        let now = Utc::now().to_rfc3339();
        let embedding_json = serde_json::to_string(vector)?;

        let mut tx = self.pool.begin().await?;

        // vec0 tables don't upsert; replace by delete+insert. Only when the
        // sqlite-vec index actually exists.
        if self.ai_status.is_ready() {
            sqlx::query("DELETE FROM vec_notes WHERE note_id = ?")
                .bind(&note_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO vec_notes(note_id, embedding) VALUES (?, ?)")
                .bind(&note_id)
                .bind(&embedding_json)
                .execute(&mut *tx)
                .await?;
        }

        sqlx::query(
            r#"
            INSERT INTO note_embeddings (note_id, model_id, dimensions, content_hash, embedded_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(note_id) DO UPDATE SET
                model_id = excluded.model_id,
                dimensions = excluded.dimensions,
                content_hash = excluded.content_hash,
                embedded_at = excluded.embedded_at
            "#,
        )
        .bind(&note_id)
        .bind(model_id)
        .bind(dimensions as i64)
        .bind(content_hash)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM embedding_queue WHERE note_id = ? AND content_hash = ?")
            .bind(&note_id)
            .bind(content_hash)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Record a failed embedding attempt: increment attempts, store the error,
    /// and set `next_attempt_at` so the job is skipped until the backoff window
    /// (computed by the caller) elapses.
    #[cfg(feature = "ai")]
    pub async fn mark_embedding_failed(
        &self,
        note_id: Uuid,
        error: &str,
        retry_after: chrono::DateTime<Utc>,
    ) -> Result<()> {
        if !self.embedding_queue_enabled() {
            return Ok(());
        }
        sqlx::query(
            "UPDATE embedding_queue \
             SET attempts = attempts + 1, last_error = ?, next_attempt_at = ? \
             WHERE note_id = ?",
        )
        .bind(error)
        .bind(retry_after.to_rfc3339())
        .bind(note_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Number of queued embedding jobs, for progress reporting.
    #[cfg(feature = "ai")]
    pub async fn count_pending_embeddings(&self) -> Result<i64> {
        if !self.embedding_queue_enabled() {
            return Ok(0);
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM embedding_queue")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Diagnostics for jobs that have failed at least once: how many are in the
    /// queue with a non-zero attempt count, and the most-retried job's last
    /// error. Surfaced by `jot-down doctor`.
    #[cfg(feature = "ai")]
    pub async fn failed_embedding_stats(&self) -> Result<(i64, Option<String>)> {
        if !self.embedding_queue_enabled() {
            return Ok((0, None));
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM embedding_queue WHERE attempts > 0")
                .fetch_one(&self.pool)
                .await?;
        let last_error: Option<String> = sqlx::query_scalar(
            "SELECT last_error FROM embedding_queue \
             WHERE attempts > 0 AND last_error IS NOT NULL \
             ORDER BY attempts DESC, enqueued_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok((count, last_error))
    }

    /// Enqueue every non-deleted note whose content hash differs from the last
    /// embedded version (or that has never been embedded). Idempotent — re-runs
    /// only add notes that are still stale.
    ///
    /// Returns the number of newly enqueued rows.
    #[cfg(feature = "ai")]
    pub async fn enqueue_all_stale_embeddings(&self) -> Result<i64> {
        if !self.embedding_queue_enabled() {
            return Ok(0);
        }
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            r#"
            INSERT INTO embedding_queue (note_id, content_hash, enqueued_at)
            SELECT n.id, n.content_hash, ?
            FROM notes n
            LEFT JOIN note_embeddings e ON e.note_id = n.id
            WHERE n.deleted_at IS NULL
              AND (e.note_id IS NULL OR e.content_hash != n.content_hash)
            ON CONFLICT(note_id) DO NOTHING
            "#,
        )
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() as i64)
    }

    /// Whether the persisted index was built with a different embedding model
    /// than the one supplied (by id or vector width). A change here means every
    /// stored vector is stale and the index must be rebuilt from scratch — the
    /// stored vectors are not comparable across models. False when the AI
    /// bookkeeping tables aren't present.
    #[cfg(feature = "ai")]
    pub async fn embedding_model_changed(
        &self,
        model_id: &str,
        dimensions: usize,
    ) -> Result<bool> {
        if !self.embedding_queue_enabled() {
            return Ok(false);
        }
        let stored_id = self.get_metadata("embedding_model_id").await?;
        let stored_dims = self.get_metadata("embedding_dimensions").await?;
        Ok(stored_id.as_deref() != Some(model_id)
            || stored_dims.as_deref() != Some(dimensions.to_string().as_str()))
    }

    /// Rebuild the embedding index from scratch for a (possibly new) model:
    /// drop every stored vector and all embedding bookkeeping, record the model
    /// the index is now keyed on, and re-enqueue every live note so the worker
    /// re-embeds it. Returns the number of notes enqueued.
    ///
    /// Transactional: either the index is fully reset and re-enqueued, or
    /// nothing changes. Used both by the automatic startup reindex (model
    /// changed) and the user-initiated rebuild.
    #[cfg(feature = "ai")]
    pub async fn reindex_embeddings(&self, model_id: &str, dimensions: usize) -> Result<i64> {
        if !self.embedding_queue_enabled() {
            return Ok(0);
        }
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin().await?;

        // Clear bookkeeping + the work queue (these tables exist whenever the
        // AI migrations have run).
        sqlx::query("DELETE FROM note_embeddings")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM embedding_queue")
            .execute(&mut *tx)
            .await?;
        // The vec0 index only exists when sqlite-vec actually loaded.
        if self.ai_status.is_ready() {
            sqlx::query("DELETE FROM vec_notes").execute(&mut *tx).await?;
        }

        // Re-enqueue every non-deleted note for the new model.
        let result = sqlx::query(
            r#"
            INSERT INTO embedding_queue (note_id, content_hash, enqueued_at)
            SELECT id, content_hash, ? FROM notes WHERE deleted_at IS NULL
            "#,
        )
        .bind(&now)
        .execute(&mut *tx)
        .await?;

        // Record the model the index is now built for, so a later run can tell
        // whether another switch happened.
        for (key, value) in [
            ("embedding_model_id", model_id.to_string()),
            ("embedding_dimensions", dimensions.to_string()),
        ] {
            sqlx::query(
                "INSERT INTO local_metadata (key, value) VALUES (?, ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(result.rows_affected() as i64)
    }

    /// Nearest note ids to a query embedding, by vector distance (closest
    /// first). Empty when the vector index isn't ready.
    #[cfg(feature = "ai")]
    pub async fn semantic_note_ids(&self, embedding: &[f32], k: i64) -> Result<Vec<Uuid>> {
        if !self.ai_status.is_ready() {
            return Ok(Vec::new());
        }
        let json = serde_json::to_string(embedding)?;
        let rows = sqlx::query(
            "SELECT note_id FROM vec_notes WHERE embedding MATCH ? ORDER BY distance LIMIT ?",
        )
        .bind(json)
        .bind(k.max(1))
        .fetch_all(&self.pool)
        .await?;

        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.get("note_id");
            if let Ok(uuid) = Uuid::parse_str(&id) {
                ids.push(uuid);
            }
        }
        Ok(ids)
    }

    /// Nearest-neighbor note ids for an already-embedded note, ordered by
    /// vector distance (closest first), excluding the note itself. Returns up
    /// to `k` ids. Empty when the vector index isn't ready or the note has no
    /// stored embedding.
    ///
    /// This runs the KNN search directly against the note's own stored vector
    /// via a subquery, so the embedding never round-trips through the app (the
    /// vec0 `embedding` column is a raw float blob, not JSON).
    #[cfg(feature = "ai")]
    async fn neighbors_of_note(&self, note_id: Uuid, k: i64) -> Result<Vec<Uuid>> {
        if !self.ai_status.is_ready() {
            return Ok(Vec::new());
        }

        // Guard: without a stored vector the MATCH subquery yields NULL, which
        // sqlite-vec rejects. Bail out cleanly instead.
        let has_vector: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM vec_notes WHERE note_id = ?")
                .bind(note_id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        if has_vector.is_none() {
            return Ok(Vec::new());
        }

        // k+1 because the note itself is always its own closest match.
        let rows = sqlx::query(
            "SELECT note_id FROM vec_notes \
             WHERE embedding MATCH (SELECT embedding FROM vec_notes WHERE note_id = ?) \
             ORDER BY distance LIMIT ?",
        )
        .bind(note_id.to_string())
        .bind((k + 1).max(1))
        .fetch_all(&self.pool)
        .await?;

        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.get("note_id");
            if let Ok(uuid) = Uuid::parse_str(&id) {
                if uuid != note_id {
                    ids.push(uuid);
                }
            }
        }
        ids.truncate(k as usize);
        Ok(ids)
    }

    /// Related notes: nearest neighbors by vector distance, excluding the note
    /// itself. Returns up to `k` summaries ordered by semantic similarity.
    /// Empty when the vector index isn't ready or the note has no embedding.
    #[cfg(feature = "ai")]
    pub async fn related_notes(&self, note_id: Uuid, k: i64) -> Result<Vec<NoteSummary>> {
        let ids = self.neighbors_of_note(note_id, k).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // summaries_for_ids returns rows in unspecified order; restore the
        // similarity ranking from the neighbor query.
        let order: HashMap<Uuid, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let mut summaries = self.summaries_for_ids(&ids).await?;
        summaries.sort_by_key(|s| order.get(&s.id).copied().unwrap_or(usize::MAX));
        Ok(summaries)
    }

    /// Suggest tags for a note by examining nearest-neighbor tags.
    /// Returns up to `max_tags` tag names ordered by frequency among neighbors,
    /// excluding tags already on the note. Empty when AI is unavailable or the
    /// note has no embedding.
    #[cfg(feature = "ai")]
    pub async fn suggest_tags_for_note(
        &self,
        note_id: Uuid,
        k_neighbors: i64,
        max_tags: usize,
    ) -> Result<Vec<String>> {
        let ids = self.neighbors_of_note(note_id, k_neighbors).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // Get existing tags on the target note to exclude them.
        let existing_tags = self.get_tags_for_note(note_id).await?;

        // Collect all tags from neighbors.
        let mut tag_counts: HashMap<String, usize> = HashMap::new();
        for neighbor_id in &ids {
            let tags = self.get_tags_for_note(*neighbor_id).await?;
            for tag in tags {
                if !existing_tags.contains(&tag) {
                    *tag_counts.entry(tag).or_default() += 1;
                }
            }
        }

        // Sort by frequency descending, then alphabetically.
        let mut candidates: Vec<(String, usize)> = tag_counts.into_iter().collect();
        candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        Ok(candidates.into_iter().take(max_tags).map(|(tag, _)| tag).collect())
    }

    /// Hydrate note summaries for a set of ids (skipping soft-deleted notes).
    /// Order is unspecified; callers re-order as needed.
    #[cfg(feature = "ai")]
    pub async fn summaries_for_ids(&self, ids: &[Uuid]) -> Result<Vec<NoteSummary>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT id, title, created_at, updated_at FROM notes \
             WHERE deleted_at IS NULL AND id IN ({placeholders})"
        );
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(id.to_string());
        }
        let rows = query.fetch_all(&self.pool).await?;

        let mut notes = Vec::with_capacity(rows.len());
        for row in rows {
            let id = Uuid::parse_str(row.get::<&str, _>("id"))?;
            let tags = self.get_tags_for_note(id).await?;
            notes.push(NoteSummary {
                id,
                title: row.get("title"),
                created_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))?
                    .with_timezone(&Utc),
                updated_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))?
                    .with_timezone(&Utc),
                tags,
            });
        }
        Ok(notes)
    }

    // -----------------------------------------------------------------------
    // Note CRUD
    // -----------------------------------------------------------------------

    /// Create a new note.
    pub async fn create_note(&self, input: CreateNoteInput) -> Result<Note> {
        let id = Uuid::new_v4();
        let now = Utc::now();

        let mut note = Note {
            id,
            title: input.title,
            body: input.body,
            created_at: now,
            updated_at: now,
            tags: Vec::new(),
            content_hash: String::new(),
            remote_version: 0,
        };
        note.content_hash = note.compute_content_hash();

        sqlx::query(
            "INSERT INTO notes (id, title, body, created_at, updated_at, content_hash) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&note.title)
        .bind(&note.body)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(&note.content_hash)
        .execute(&self.pool)
        .await?;

        self.enqueue_note_create(&note).await?;
        self.enqueue_embedding_if_stale(note.id, &note.content_hash)
            .await?;

        // Index this note's outgoing links, and resolve any dangling links that
        // were waiting for a note with this title to exist.
        self.reindex_note_links(note.id, &note.body).await?;
        self.resolve_links_to_title(note.id, &note.title).await?;

        Ok(note)
    }

    /// Update an existing note.
    pub async fn update_note(&self, input: UpdateNoteInput) -> Result<Note> {
        let now = Utc::now();

        // Fetch the current remote_version before updating
        let current_version: i32 = sqlx::query_scalar(
            "SELECT COALESCE(remote_version, 0) FROM notes WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(input.id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0);

        let tags = self.get_tags_for_note(input.id).await?;

        let mut note = Note {
            id: input.id,
            title: input.title.clone(),
            body: input.body.clone(),
            created_at: now,
            updated_at: now,
            tags,
            content_hash: String::new(),
            remote_version: current_version,
        };
        note.content_hash = note.compute_content_hash();

        let rows = sqlx::query(
            "UPDATE notes SET title = ?, body = ?, updated_at = ?, content_hash = ? WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(&input.title)
        .bind(&input.body)
        .bind(now.to_rfc3339())
        .bind(&note.content_hash)
        .bind(input.id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();

        if rows == 0 {
            anyhow::bail!("Note not found or has been deleted: {}", input.id);
        }

        self.enqueue_note_update(input.id, &input).await?;
        self.enqueue_embedding_if_stale(note.id, &note.content_hash)
            .await?;

        // Re-index outgoing links from the new body, and (if the title changed)
        // resolve dangling links that now point at this note.
        self.reindex_note_links(note.id, &note.body).await?;
        self.resolve_links_to_title(note.id, &note.title).await?;

        Ok(note)
    }

    /// Soft-delete a note by setting its deleted_at timestamp.
    pub async fn soft_delete_note(&self, note_id: Uuid) -> Result<()> {
        let now = Utc::now();
        let rows =
            sqlx::query("UPDATE notes SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
                .bind(now.to_rfc3339())
                .bind(note_id.to_string())
                .execute(&self.pool)
                .await?
                .rows_affected();

        if rows == 0 {
            anyhow::bail!("Note not found or already deleted: {}", note_id);
        }

        self.enqueue_note_delete(note_id).await?;
        self.remove_embedding_state(note_id).await?;

        Ok(())
    }

    /// List soft-deleted notes (the trash), most recently deleted first.
    pub async fn list_deleted_notes(&self) -> Result<Vec<NoteSummary>> {
        let rows = sqlx::query(
            r#"
            SELECT n.id, n.title, n.created_at, n.updated_at
            FROM notes n
            WHERE n.deleted_at IS NOT NULL
            ORDER BY n.deleted_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut notes = Vec::with_capacity(rows.len());
        for row in rows {
            let id = Uuid::parse_str(row.get::<&str, _>("id"))?;
            let tags = self.get_tags_for_note(id).await?;
            notes.push(NoteSummary {
                id,
                title: row.get("title"),
                created_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))?
                    .with_timezone(&Utc),
                updated_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))?
                    .with_timezone(&Utc),
                tags,
            });
        }
        Ok(notes)
    }

    /// Restore a soft-deleted note: clear its `deleted_at`, propagate the
    /// revival to other devices as an update, and re-enqueue it for embedding.
    pub async fn restore_note(&self, note_id: Uuid) -> Result<()> {
        let now = Utc::now();
        let rows = sqlx::query(
            "UPDATE notes SET deleted_at = NULL, updated_at = ? \
             WHERE id = ? AND deleted_at IS NOT NULL",
        )
        .bind(now.to_rfc3339())
        .bind(note_id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();

        if rows == 0 {
            anyhow::bail!("Note not found in trash: {}", note_id);
        }

        // Fetch the restored content to propagate + re-index.
        let row = sqlx::query("SELECT title, body, content_hash FROM notes WHERE id = ?")
            .bind(note_id.to_string())
            .fetch_one(&self.pool)
            .await?;
        let title: String = row.get("title");
        let body: String = row.get("body");
        let content_hash: String = row.get("content_hash");

        // An "update" is the closest existing sync primitive for "this note is
        // live again with this content".
        self.enqueue_note_update(note_id, &UpdateNoteInput { id: note_id, title, body })
            .await?;
        self.enqueue_embedding_if_stale(note_id, &content_hash).await?;

        Ok(())
    }

    /// Permanently delete a note that is already in the trash, along with its
    /// tag links. Refuses to touch a live note. Embedding/queue rows were
    /// already cleared at soft-delete time and cascade on the row delete.
    pub async fn purge_note(&self, note_id: Uuid) -> Result<()> {
        let id = note_id.to_string();
        let mut tx = self.pool.begin().await?;

        let trashed: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM notes WHERE id = ? AND deleted_at IS NOT NULL")
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await?;
        if trashed.is_none() {
            anyhow::bail!("Note not found in trash: {}", note_id);
        }

        // note_tags has no ON DELETE CASCADE, so clear links first.
        sqlx::query("DELETE FROM note_tags WHERE note_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM notes WHERE id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Whether a live note with this exact title and body already exists. Used
    /// to dedupe Markdown imports so re-importing the same file is a no-op.
    pub async fn live_note_exists_with_content(&self, title: &str, body: &str) -> Result<bool> {
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM notes WHERE deleted_at IS NULL AND title = ? AND body = ? LIMIT 1",
        )
        .bind(title)
        .bind(body)
        .fetch_optional(&self.pool)
        .await?;
        Ok(exists.is_some())
    }

    /// The most-recently-updated live note with this exact title, if any.
    async fn live_note_id_by_title(&self, title: &str) -> Result<Option<Uuid>> {
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM notes WHERE deleted_at IS NULL AND title = ? \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(title)
        .fetch_optional(&self.pool)
        .await?;
        Ok(id.map(|s| Uuid::parse_str(&s)).transpose()?)
    }

    /// Open-or-create today's note. Returns (id, created). `created` is true when a
    /// new note was made. Reuses `create_note`, so it enqueues sync + embedding the
    /// same way a normal note does.
    ///
    /// `daily_date` is the ISO `YYYY-MM-DD` calendar day this note represents
    /// (stored as the typed marker, independent of the title format). When
    /// `rollup` is true and a new note is created, the unfinished `- [ ]` tasks
    /// from the most recent *prior* daily note are carried forward under a
    /// `## Carried over` heading. Rollup happens exactly once, at creation, so
    /// reopening never duplicates.
    pub async fn find_or_create_daily_note(
        &self,
        title: &str,
        template: &str,
        daily_date: &str,
        rollup: bool,
    ) -> Result<(Uuid, bool)> {
        if let Some(id) = self.live_note_id_by_title(title).await? {
            return Ok((id, false));
        }

        let mut body = template.to_string();
        if rollup {
            if let Some(prior) = self.most_recent_prior_daily_body(daily_date).await? {
                let tasks = crate::notes::daily::unfinished_tasks(&prior);
                body = crate::notes::daily::append_carried_over(&body, &tasks);
            }
        }

        let note = self
            .create_note(CreateNoteInput {
                title: title.to_string(),
                body,
            })
            .await?;
        self.set_daily_date(note.id, daily_date).await?;
        Ok((note.id, true))
    }

    /// Body of the most recent live daily note dated strictly before `today`
    /// (ISO `YYYY-MM-DD`). ISO dates sort lexicographically by calendar order,
    /// so a plain string comparison is correct.
    async fn most_recent_prior_daily_body(&self, today: &str) -> Result<Option<String>> {
        let body: Option<String> = sqlx::query_scalar(
            "SELECT body FROM notes \
             WHERE deleted_at IS NULL AND daily_date IS NOT NULL AND daily_date < ? \
             ORDER BY daily_date DESC LIMIT 1",
        )
        .bind(today)
        .fetch_optional(&self.pool)
        .await?;
        Ok(body)
    }

    /// Tag a note with its daily calendar date. Deliberately does not touch
    /// `updated_at` — this is local-only metadata, not a content edit, so it
    /// shouldn't reorder the note list or trigger a re-sync.
    async fn set_daily_date(&self, note_id: Uuid, daily_date: &str) -> Result<()> {
        sqlx::query("UPDATE notes SET daily_date = ? WHERE id = ?")
            .bind(daily_date)
            .bind(note_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Rebuild the `note_links` rows for `src_id` from its current `body`:
    /// extract every `[[…]]` target, de-duplicate by normalized title, resolve
    /// each to a live note id where one exists (NULL = dangling), and replace
    /// the prior rows wholesale. Called on every create/update so the index
    /// tracks the body.
    async fn reindex_note_links(&self, src_id: Uuid, body: &str) -> Result<()> {
        use std::collections::HashSet;

        let mut seen = HashSet::new();
        let targets: Vec<String> = crate::notes::wikilinks::extract_wikilinks(body)
            .into_iter()
            .map(|l| crate::notes::wikilinks::normalize_title(&l.target))
            .filter(|t| !t.is_empty() && seen.insert(t.clone()))
            .collect();

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM note_links WHERE src_id = ?")
            .bind(src_id.to_string())
            .execute(&mut *tx)
            .await?;
        for title in targets {
            // Resolve to the most-recently-updated live note with this title
            // (case-insensitive). Left NULL when nothing matches yet.
            let dst_id: Option<String> = sqlx::query_scalar(
                "SELECT id FROM notes \
                 WHERE deleted_at IS NULL AND LOWER(title) = ? \
                 ORDER BY updated_at DESC LIMIT 1",
            )
            .bind(&title)
            .fetch_optional(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT OR REPLACE INTO note_links (src_id, dst_title, dst_id) VALUES (?, ?, ?)",
            )
            .bind(src_id.to_string())
            .bind(&title)
            .bind(dst_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Back-fill `dst_id` for dangling links that point at `title` — called when
    /// a note with that title is created or renamed, so existing `[[title]]`
    /// references light up without re-scanning every body.
    async fn resolve_links_to_title(&self, note_id: Uuid, title: &str) -> Result<()> {
        let norm = crate::notes::wikilinks::normalize_title(title);
        if norm.is_empty() {
            return Ok(());
        }
        sqlx::query("UPDATE note_links SET dst_id = ? WHERE dst_title = ? AND dst_id IS NULL")
            .bind(note_id.to_string())
            .bind(&norm)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Notes that link *to* `note_id` via a resolved `[[wikilink]]`, most
    /// recently updated first. Drives the preview's "Linked from" panel.
    pub async fn backlinks(&self, note_id: Uuid) -> Result<Vec<NoteSummary>> {
        let rows = sqlx::query(
            "SELECT n.id, n.title, n.created_at, n.updated_at \
             FROM note_links l JOIN notes n ON n.id = l.src_id \
             WHERE l.dst_id = ? AND n.deleted_at IS NULL \
             ORDER BY n.updated_at DESC",
        )
        .bind(note_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut notes = Vec::with_capacity(rows.len());
        for row in rows {
            let id = Uuid::parse_str(row.get::<&str, _>("id"))?;
            let tags = self.get_tags_for_note(id).await?;
            notes.push(NoteSummary {
                id,
                title: row.get("title"),
                created_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))?
                    .with_timezone(&Utc),
                updated_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))?
                    .with_timezone(&Utc),
                tags,
            });
        }
        Ok(notes)
    }

    /// Map of normalized (trimmed, ASCII-lowercased) title → note id for every
    /// live note, keeping the most-recently-updated note on a title collision.
    /// The preview uses the keys to style `[[wikilinks]]` live vs dangling and
    /// the ids to navigate (Enter-to-open).
    pub async fn live_title_index(&self) -> Result<std::collections::HashMap<String, Uuid>> {
        // Ascending so a later (more recent) row overwrites an earlier one for
        // the same normalized title.
        let rows = sqlx::query(
            "SELECT id, title FROM notes WHERE deleted_at IS NULL ORDER BY updated_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut index = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let id = Uuid::parse_str(row.get::<&str, _>("id"))?;
            let title: String = row.get("title");
            index.insert(crate::notes::wikilinks::normalize_title(&title), id);
        }
        Ok(index)
    }

    /// "On this day": for the daily note `note_id`, the daily notes from prior
    /// periods (each `offset` in days) that share the calendar day, as
    /// `(offset, summary)`. Empty when the note isn't a daily note (no
    /// `daily_date`) or nothing matches.
    pub async fn on_this_day_notes(
        &self,
        note_id: Uuid,
        offsets: &[u32],
    ) -> Result<Vec<(u32, NoteSummary)>> {
        let today: Option<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT daily_date FROM notes WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(note_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        let Some(today) = today else {
            return Ok(Vec::new());
        };

        let mut out = Vec::new();
        for (offset, date) in crate::notes::daily::on_this_day_targets(&today, offsets) {
            let rows = sqlx::query(
                "SELECT id, title, created_at, updated_at FROM notes \
                 WHERE deleted_at IS NULL AND daily_date = ? AND id != ? \
                 ORDER BY updated_at DESC",
            )
            .bind(&date)
            .bind(note_id.to_string())
            .fetch_all(&self.pool)
            .await?;
            for row in rows {
                let id = Uuid::parse_str(row.get::<&str, _>("id"))?;
                let tags = self.get_tags_for_note(id).await?;
                out.push((
                    offset,
                    NoteSummary {
                        id,
                        title: row.get("title"),
                        created_at: chrono::DateTime::parse_from_rfc3339(
                            row.get::<&str, _>("created_at"),
                        )?
                        .with_timezone(&Utc),
                        updated_at: chrono::DateTime::parse_from_rfc3339(
                            row.get::<&str, _>("updated_at"),
                        )?
                        .with_timezone(&Utc),
                        tags,
                    },
                ));
            }
        }
        Ok(out)
    }

    /// List all non-deleted notes, ordered by most recently updated.
    pub async fn list_notes(&self) -> Result<Vec<NoteSummary>> {
        let rows = sqlx::query(
            r#"
            SELECT n.id, n.title, n.created_at, n.updated_at
            FROM notes n
            WHERE n.deleted_at IS NULL
            ORDER BY n.updated_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut notes = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.get("id");
            let id = Uuid::parse_str(&id_str)?;
            let tags = self.get_tags_for_note(id).await?;

            notes.push(NoteSummary {
                id,
                title: row.get("title"),
                created_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))?
                    .with_timezone(&Utc),
                updated_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))?
                    .with_timezone(&Utc),
                tags,
            });
        }

        Ok(notes)
    }

    /// Get a single note by ID.
    pub async fn get_note(&self, note_id: Uuid) -> Result<Option<Note>> {
        let row = sqlx::query(
            "SELECT id, title, body, created_at, updated_at, COALESCE(content_hash, '') AS content_hash, COALESCE(remote_version, 0) AS remote_version FROM notes WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(note_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        let row = match row {
            Some(r) => r,
            None => return Ok(None),
        };

        let id_str: String = row.get("id");
        let id = Uuid::parse_str(&id_str)?;
        let tags = self.get_tags_for_note(id).await?;

        Ok(Some(Note {
            id,
            title: row.get("title"),
            body: row.get("body"),
            created_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))?
                .with_timezone(&Utc),
            updated_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))?
                .with_timezone(&Utc),
            tags,
            content_hash: row.get("content_hash"),
            remote_version: row.get("remote_version"),
        }))
    }

    /// List all non-deleted notes that have the given tag.
    #[allow(dead_code)]
    pub async fn list_notes_by_tag(&self, tag_name: &str) -> Result<Vec<NoteSummary>> {
        let rows = sqlx::query(
            r#"
            SELECT n.id, n.title, n.created_at, n.updated_at
            FROM notes n
            JOIN note_tags nt ON nt.note_id = n.id
            JOIN tags t ON t.id = nt.tag_id
            WHERE n.deleted_at IS NULL AND t.name = ?
            ORDER BY n.updated_at DESC
            "#,
        )
        .bind(tag_name)
        .fetch_all(&self.pool)
        .await?;

        let mut notes = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.get("id");
            let id = Uuid::parse_str(&id_str)?;
            let tags = self.get_tags_for_note(id).await?;

            notes.push(NoteSummary {
                id,
                title: row.get("title"),
                created_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))?
                    .with_timezone(&Utc),
                updated_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))?
                    .with_timezone(&Utc),
                tags,
            });
        }

        Ok(notes)
    }

    /// Search notes by title or body text.
    pub async fn search_notes(&self, query: &str) -> Result<Vec<NoteSummary>> {
        let pattern = format!("%{}%", query);
        let rows = sqlx::query(
            r#"
            SELECT n.id, n.title, n.created_at, n.updated_at
            FROM notes n
            WHERE n.deleted_at IS NULL
              AND (n.title LIKE ? OR n.body LIKE ?)
            ORDER BY n.updated_at DESC
            "#,
        )
        .bind(&pattern)
        .bind(&pattern)
        .fetch_all(&self.pool)
        .await?;

        let mut notes = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.get("id");
            let id = Uuid::parse_str(&id_str)?;
            let tags = self.get_tags_for_note(id).await?;

            notes.push(NoteSummary {
                id,
                title: row.get("title"),
                created_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))?
                    .with_timezone(&Utc),
                updated_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))?
                    .with_timezone(&Utc),
                tags,
            });
        }

        Ok(notes)
    }

    // -----------------------------------------------------------------------
    // Tag CRUD
    // -----------------------------------------------------------------------

    /// Create a new tag.
    pub async fn create_tag(&self, name: &str) -> Result<Tag> {
        let id = Uuid::new_v4();
        let now = Utc::now();

        sqlx::query("INSERT INTO tags (id, name, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(id.to_string())
            .bind(name)
            .bind(now.to_rfc3339())
            .bind(now.to_rfc3339())
            .execute(&self.pool)
            .await?;

        Ok(Tag {
            id,
            name: name.to_string(),
            created_at: now,
            updated_at: now,
        })
    }

    /// Get or create a tag by name.
    pub async fn get_or_create_tag(&self, name: &str) -> Result<Tag> {
        // Try to find existing tag
        let existing =
            sqlx::query("SELECT id, name, created_at, updated_at FROM tags WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;

        if let Some(row) = existing {
            let id_str: String = row.get("id");
            let id = Uuid::parse_str(&id_str)?;
            return Ok(Tag {
                id,
                name: row.get("name"),
                created_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))?
                    .with_timezone(&Utc),
                updated_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))?
                    .with_timezone(&Utc),
            });
        }

        self.create_tag(name).await
    }

    /// List all tags.
    #[allow(dead_code)]
    pub async fn list_tags(&self) -> Result<Vec<Tag>> {
        let rows = sqlx::query("SELECT id, name, created_at, updated_at FROM tags ORDER BY name")
            .fetch_all(&self.pool)
            .await?;

        let mut tags = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.get("id");
            tags.push(Tag {
                id: Uuid::parse_str(&id_str)?,
                name: row.get("name"),
                created_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))?
                    .with_timezone(&Utc),
                updated_at: chrono::DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))?
                    .with_timezone(&Utc),
            });
        }

        Ok(tags)
    }

    /// Add a tag to a note.
    pub async fn add_tag_to_note(&self, note_id: Uuid, tag_name: &str) -> Result<Tag> {
        let tag = self.get_or_create_tag(tag_name).await?;

        sqlx::query("INSERT OR IGNORE INTO note_tags (note_id, tag_id) VALUES (?, ?)")
            .bind(note_id.to_string())
            .bind(tag.id.to_string())
            .execute(&self.pool)
            .await?;

        self.enqueue_tag_add(note_id, tag.id, tag_name).await?;

        Ok(tag)
    }

    /// Remove a tag from a note.
    pub async fn remove_tag_from_note(&self, note_id: Uuid, tag_id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM note_tags WHERE note_id = ? AND tag_id = ?")
            .bind(note_id.to_string())
            .bind(tag_id.to_string())
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Remove a tag from a note by tag name (looks up tag ID automatically).
    pub async fn remove_tag_by_name(&self, note_id: Uuid, tag_name: &str) -> Result<()> {
        let tag = self.get_or_create_tag(tag_name).await?;
        self.remove_tag_from_note(note_id, tag.id).await?;
        self.enqueue_tag_remove(note_id, tag.id, tag_name).await?;
        Ok(())
    }

    /// Apply a tag-add pulled from a remote device. Resolves the tag by name
    /// (tag ids differ per device) and does NOT enqueue a sync op — this is an
    /// inbound change, so re-pushing it would echo back to the origin.
    pub async fn apply_remote_tag_add(&self, note_id: Uuid, tag_name: &str) -> Result<()> {
        let tag = self.get_or_create_tag(tag_name).await?;
        sqlx::query("INSERT OR IGNORE INTO note_tags (note_id, tag_id) VALUES (?, ?)")
            .bind(note_id.to_string())
            .bind(tag.id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Apply a tag-remove pulled from a remote device (no sync enqueue).
    pub async fn apply_remote_tag_remove(&self, note_id: Uuid, tag_name: &str) -> Result<()> {
        let existing = sqlx::query("SELECT id FROM tags WHERE name = ?")
            .bind(tag_name)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(row) = existing {
            let id_str: String = row.get("id");
            let tag_id = Uuid::parse_str(&id_str)?;
            self.remove_tag_from_note(note_id, tag_id).await?;
        }
        Ok(())
    }

    /// Read a value from `local_metadata` by key.
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    pub async fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        let value = sqlx::query_scalar("SELECT value FROM local_metadata WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(value)
    }

    /// Get or create a unique device ID stored in local_metadata.
    pub async fn get_or_create_device_id(&self) -> Result<Uuid> {
        // Try to get existing
        let row = sqlx::query("SELECT value FROM local_metadata WHERE key = 'device_id'")
            .fetch_optional(&self.pool)
            .await?;

        if let Some(r) = row {
            let val: String = r.get("value");
            return Ok(Uuid::parse_str(&val)?);
        }

        // Create new
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO local_metadata (key, value) VALUES ('device_id', ?)")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;

        Ok(id)
    }

    /// Get all pending sync queue entries.
    pub async fn get_sync_queue_entries(&self, limit: usize) -> Result<Vec<SyncQueueEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT id, entity_type, entity_id, operation, payload_json, attempts, last_error
            FROM sync_queue
            ORDER BY id ASC
            LIMIT ?
            "#,
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let payload_str: String = row.get("payload_json");
            let payload: serde_json::Value = serde_json::from_str(&payload_str)?;
            entries.push(SyncQueueEntry {
                id: row.get("id"),
                entity_type: row.get("entity_type"),
                entity_id: row.get("entity_id"),
                operation: row.get("operation"),
                payload_json: payload,
                attempts: row.get("attempts"),
                last_error: row.get("last_error"),
            });
        }

        Ok(entries)
    }

    /// Get sync queue entries that are due for a push attempt — i.e. that have
    /// never failed, or whose backoff window (`next_attempt_at`) has elapsed.
    pub async fn get_due_sync_queue_entries(&self, limit: usize) -> Result<Vec<SyncQueueEntry>> {
        let now = Utc::now().to_rfc3339();
        let rows = sqlx::query(
            r#"
            SELECT id, entity_type, entity_id, operation, payload_json, attempts, last_error
            FROM sync_queue
            WHERE next_attempt_at IS NULL OR next_attempt_at <= ?
            ORDER BY id ASC
            LIMIT ?
            "#,
        )
        .bind(&now)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let payload_str: String = row.get("payload_json");
            let payload: serde_json::Value = serde_json::from_str(&payload_str)?;
            entries.push(SyncQueueEntry {
                id: row.get("id"),
                entity_type: row.get("entity_type"),
                entity_id: row.get("entity_id"),
                operation: row.get("operation"),
                payload_json: payload,
                attempts: row.get("attempts"),
                last_error: row.get("last_error"),
            });
        }

        Ok(entries)
    }

    /// Clear all retry backoff so every queued entry is immediately due. Used
    /// when the user triggers a manual sync.
    pub async fn reset_sync_backoff(&self) -> Result<()> {
        sqlx::query("UPDATE sync_queue SET next_attempt_at = NULL")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Compact redundant sync queue entries.
    ///
    /// Rules:
    /// - If a "delete" exists for an entity, remove all other entries for that entity.
    /// - If multiple "update"/"create" entries exist for an entity, keep only the
    ///   create (if any) and the most recent update.
    /// - Never compact across operations — the entity_type/entity_id scope is sufficient.
    pub async fn compact_sync_queue(&self) -> Result<usize> {
        let mut total_removed = 0usize;

        // Step 1: find entities where the latest entry is "delete" — remove all prior entries
        let rows: Vec<(i64,)> = sqlx::query_as(
            r#"
            SELECT sq_keep.id
            FROM sync_queue sq_keep
            WHERE sq_keep.operation = 'delete'
              AND sq_keep.id = (
                  SELECT MAX(sq_inner.id)
                  FROM sync_queue sq_inner
                  WHERE sq_inner.entity_type = sq_keep.entity_type
                    AND sq_inner.entity_id = sq_keep.entity_id
              )
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        for (delete_id,) in &rows {
            // Delete all earlier entries for the same entity
            let removed = sqlx::query(
                r#"
                DELETE FROM sync_queue
                WHERE entity_type = (SELECT entity_type FROM sync_queue WHERE id = ?)
                  AND entity_id = (SELECT entity_id FROM sync_queue WHERE id = ?)
                  AND id < ?
                "#,
            )
            .bind(delete_id)
            .bind(delete_id)
            .bind(delete_id)
            .execute(&self.pool)
            .await?
            .rows_affected() as usize;
            total_removed += removed;
        }

        // Step 2: for entities with multiple updates (no delete), keep only latest update
        // We keep the CREATE if it exists, and the latest UPDATE. Remove others.
        let groups: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT entity_type, entity_id
            FROM sync_queue
            WHERE operation IN ('create', 'update')
            GROUP BY entity_type, entity_id
            HAVING COUNT(*) > 1
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        for (entity_type, entity_id) in &groups {
            // Find the max id for this group (the latest entry)
            let max_id: Option<(i64,)> = sqlx::query_as(
                "SELECT MAX(id) FROM sync_queue WHERE entity_type = ? AND entity_id = ? AND operation IN ('create', 'update')",
            )
            .bind(entity_type)
            .bind(entity_id)
            .fetch_optional(&self.pool)
            .await?;

            let (latest_id,) = match max_id {
                Some(v) => v,
                None => continue,
            };

            // Find create entry id (if any) — we keep that too
            let create_id: Option<(i64,)> = sqlx::query_as(
                "SELECT id FROM sync_queue WHERE entity_type = ? AND entity_id = ? AND operation = 'create' ORDER BY id ASC LIMIT 1",
            )
            .bind(entity_type)
            .bind(entity_id)
            .fetch_optional(&self.pool)
            .await?;

            // Delete entries that are neither the latest update nor the create
            let removed = if let Some((cid,)) = create_id {
                sqlx::query(
                    r#"
                    DELETE FROM sync_queue
                    WHERE entity_type = ? AND entity_id = ?
                      AND id != ? AND id != ?
                      AND operation IN ('create', 'update')
                    "#,
                )
                .bind(entity_type)
                .bind(entity_id)
                .bind(latest_id)
                .bind(cid)
                .execute(&self.pool)
                .await?
                .rows_affected() as usize
            } else {
                // No create — keep only the latest update
                sqlx::query(
                    r#"
                    DELETE FROM sync_queue
                    WHERE entity_type = ? AND entity_id = ?
                      AND id != ?
                      AND operation IN ('create', 'update')
                    "#,
                )
                .bind(entity_type)
                .bind(entity_id)
                .bind(latest_id)
                .execute(&self.pool)
                .await?
                .rows_affected() as usize
            };
            total_removed += removed;
        }

        if total_removed > 0 {
            tracing::info!("Compacted {} redundant sync queue entries", total_removed);
        }

        Ok(total_removed)
    }

    /// Remove a sync queue entry after successful processing.
    pub async fn remove_sync_queue_entry(&self, entry_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM sync_queue WHERE id = ?")
            .bind(entry_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Mark a sync queue entry as failed: increment attempts, store the error,
    /// and set `next_attempt_at` so the entry is skipped until the backoff
    /// window (computed by the caller) elapses.
    pub async fn mark_sync_queue_error(
        &self,
        entry_id: i64,
        error: &str,
        retry_after: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            UPDATE sync_queue
            SET attempts = attempts + 1, last_error = ?, updated_at = ?, next_attempt_at = ?
            WHERE id = ?
            "#,
        )
        .bind(error)
        .bind(&now)
        .bind(retry_after.to_rfc3339())
        .bind(entry_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Conflict resolution methods ─────────────────────────────────────

    /// Create a new conflict record.
    pub async fn create_conflict(
        &self,
        note_id: Uuid,
        local_payload: serde_json::Value,
        remote_payload: serde_json::Value,
        base_version: i32,
    ) -> Result<LocalConflict> {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO local_conflicts (id, note_id, local_payload_json, remote_payload_json,
                                         base_version, detected_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(id.to_string())
        .bind(note_id.to_string())
        .bind(local_payload.to_string())
        .bind(remote_payload.to_string())
        .bind(base_version)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        tracing::info!(
            "Conflict recorded for note {} (base_version={})",
            note_id,
            base_version
        );

        Ok(LocalConflict {
            id,
            note_id,
            local_payload,
            remote_payload,
            base_version,
            detected_at: chrono::DateTime::parse_from_rfc3339(&now)
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| Utc::now()),
            resolved_at: None,
            resolution: None,
        })
    }

    /// List all unresolved conflicts.
    pub async fn list_conflicts(&self) -> Result<Vec<LocalConflict>> {
        let rows = sqlx::query(
            r#"
            SELECT id, note_id, local_payload_json, remote_payload_json,
                   base_version, detected_at, resolved_at, resolution
            FROM local_conflicts
            WHERE resolved_at IS NULL
            ORDER BY detected_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut conflicts = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.get("id");
            let note_id_str: String = row.get("note_id");
            let local_str: String = row.get("local_payload_json");
            let remote_str: String = row.get("remote_payload_json");
            let detected_str: String = row.get("detected_at");
            let resolved_str: Option<String> = row.get("resolved_at");

            conflicts.push(LocalConflict {
                id: Uuid::parse_str(&id_str).unwrap_or_default(),
                note_id: Uuid::parse_str(&note_id_str).unwrap_or_default(),
                local_payload: serde_json::from_str(&local_str).unwrap_or_default(),
                remote_payload: serde_json::from_str(&remote_str).unwrap_or_default(),
                base_version: row.get("base_version"),
                detected_at: chrono::DateTime::parse_from_rfc3339(&detected_str)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| Utc::now()),
                resolved_at: resolved_str.and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|d| d.with_timezone(&chrono::Utc))
                }),
                resolution: row.get("resolution"),
            });
        }

        Ok(conflicts)
    }

    /// Resolve a conflict with the given resolution strategy.
    pub async fn resolve_conflict(&self, conflict_id: Uuid, resolution: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let rows = sqlx::query(
            r#"
            UPDATE local_conflicts
            SET resolved_at = ?, resolution = ?
            WHERE id = ? AND resolved_at IS NULL
            "#,
        )
        .bind(&now)
        .bind(resolution)
        .bind(conflict_id.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();

        if rows == 0 {
            anyhow::bail!("Conflict not found or already resolved: {}", conflict_id);
        }

        tracing::info!(
            "Conflict {} resolved with strategy: {}",
            conflict_id,
            resolution
        );
        Ok(())
    }

    /// Count unresolved conflicts.
    pub async fn count_unresolved_conflicts(&self) -> Result<usize> {
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM local_conflicts WHERE resolved_at IS NULL")
                .fetch_one(&self.pool)
                .await?;

        Ok(count.0 as usize)
    }

    /// Upsert a note from a remote sync operation (create or update).
    ///
    /// The remote's `created_at`/`updated_at` are preserved so note ordering is
    /// consistent across devices (a pulled note keeps its real edit time rather
    /// than jumping to the top of the receiving device's list).
    pub async fn upsert_note_from_remote(
        &self,
        id: Uuid,
        title: &str,
        body: &str,
        created_at: chrono::DateTime<Utc>,
        updated_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let created = created_at.to_rfc3339();
        let updated = updated_at.to_rfc3339();

        // Compute the content hash so subsequent pulls recognise the note as
        // already-applied (otherwise every poll re-detects it as changed).
        let content_hash = Note {
            id,
            title: title.to_string(),
            body: body.to_string(),
            created_at,
            updated_at,
            tags: Vec::new(),
            content_hash: String::new(),
            remote_version: 0,
        }
        .compute_content_hash();

        // Check if note exists locally
        let exists = sqlx::query("SELECT id FROM notes WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .is_some();

        if exists {
            sqlx::query(
                "UPDATE notes SET title = ?, body = ?, updated_at = ?, content_hash = ? WHERE id = ?",
            )
            .bind(title)
            .bind(body)
            .bind(&updated)
            .bind(&content_hash)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO notes (id, title, body, created_at, updated_at, content_hash) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(id.to_string())
            .bind(title)
            .bind(body)
            .bind(&created)
            .bind(&updated)
            .bind(&content_hash)
            .execute(&self.pool)
            .await?;
        }

        self.enqueue_embedding_if_stale(id, &content_hash).await?;

        Ok(())
    }

    /// Get all tags for a note.
    async fn get_tags_for_note(&self, note_id: Uuid) -> Result<Vec<String>> {
        let rows = sqlx::query(
            r#"
            SELECT t.name
            FROM tags t
            JOIN note_tags nt ON nt.tag_id = t.id
            WHERE nt.note_id = ?
            ORDER BY t.name
            "#,
        )
        .bind(note_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| r.get::<&str, _>("name").to_string())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::note::CreateNoteInput;

    /// Create a temporary in-memory SQLite storage for testing.
    ///
    /// Uses the real migration runner so tests exercise the same schema the app
    /// builds at runtime (and stay in sync as migrations are added).
    async fn test_storage() -> SqliteStorage {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(":memory:")
                    .create_if_missing(true),
            )
            .await
            .expect("Failed to create in-memory SQLite pool");

        let storage = SqliteStorage {
            pool,
            ai_status: AiSqliteStatus::VectorExtensionUnavailable {
                error: "sqlite-vec not loaded in unit test storage".to_string(),
            },
        };
        storage
            .run_migrations()
            .await
            .expect("Failed to run migrations");
        storage
    }

    #[cfg(feature = "ai")]
    async fn queued_embedding_hash(storage: &SqliteStorage, note_id: Uuid) -> Option<String> {
        sqlx::query_scalar("SELECT content_hash FROM embedding_queue WHERE note_id = ?")
            .bind(note_id.to_string())
            .fetch_optional(&storage.pool)
            .await
            .expect("embedding queue lookup")
    }

    #[cfg(feature = "ai")]
    async fn embedding_queue_count(storage: &SqliteStorage, note_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM embedding_queue WHERE note_id = ?")
            .bind(note_id.to_string())
            .fetch_one(&storage.pool)
            .await
            .expect("embedding queue count")
    }

    #[cfg(feature = "ai")]
    async fn mark_note_embedded(storage: &SqliteStorage, note_id: Uuid, content_hash: &str) {
        sqlx::query(
            r#"
            INSERT INTO note_embeddings (note_id, model_id, dimensions, content_hash, embedded_at)
            VALUES (?, 'test-model', 384, ?, ?)
            ON CONFLICT(note_id) DO UPDATE SET
                model_id = excluded.model_id,
                dimensions = excluded.dimensions,
                content_hash = excluded.content_hash,
                embedded_at = excluded.embedded_at
            "#,
        )
        .bind(note_id.to_string())
        .bind(content_hash)
        .bind(Utc::now().to_rfc3339())
        .execute(&storage.pool)
        .await
        .expect("mark note embedded");
    }

    #[tokio::test]
    async fn test_create_and_list_note() {
        let storage = test_storage().await;

        let note = storage
            .create_note(CreateNoteInput {
                title: "Test Note".to_string(),
                body: "Hello, world!".to_string(),
            })
            .await
            .expect("Failed to create note");

        assert_eq!(note.title, "Test Note");
        assert_eq!(note.body, "Hello, world!");

        let notes = storage.list_notes().await.expect("Failed to list notes");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "Test Note");
    }

    #[tokio::test]
    async fn soft_delete_then_restore_brings_note_back() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Recoverable".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");
        storage.add_tag_to_note(note.id, "keep").await.expect("tag");

        storage.soft_delete_note(note.id).await.expect("delete");
        assert!(storage.list_notes().await.expect("list").is_empty());

        let trash = storage.list_deleted_notes().await.expect("trash");
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].id, note.id);

        storage.restore_note(note.id).await.expect("restore");
        let live = storage.list_notes().await.expect("list");
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, note.id);
        assert!(storage.list_deleted_notes().await.expect("trash").is_empty());
        // Tags survive the round-trip.
        assert_eq!(live[0].tags, vec!["keep".to_string()]);
    }

    #[tokio::test]
    async fn find_or_create_daily_note_creates_and_reopens() {
        let storage = test_storage().await;
        let title = "2026-06-17";
        let template = "Tasks:\n- ";

        // First call creates
        let (id1, created1) = storage
            .find_or_create_daily_note(title, template, "2026-06-17", false)
            .await
            .expect("first daily");
        assert!(created1, "should create on first call");

        let note = storage.get_note(id1).await.expect("get").expect("exists");
        assert_eq!(note.title, title);
        assert_eq!(note.body, template);

        // Second call reuses (idempotent)
        let (id2, created2) = storage
            .find_or_create_daily_note(title, template, "2026-06-17", false)
            .await
            .expect("second daily");
        assert!(!created2, "should not create on second call");
        assert_eq!(id1, id2, "same id on reopen");
    }

    #[tokio::test]
    async fn find_or_create_daily_note_ignores_soft_deleted() {
        let storage = test_storage().await;
        let title = "2026-06-17-old";

        // Create, then soft-delete
        let note = storage
            .find_or_create_daily_note(title, "", "2026-06-17", false)
            .await
            .expect("create");
        assert!(note.1, "created");
        storage
            .soft_delete_note(note.0)
            .await
            .expect("soft-delete");

        // Should create a fresh note since the old one is deleted
        let (new_id, created) = storage
            .find_or_create_daily_note(title, "fresh", "2026-06-17", false)
            .await
            .expect("second daily");
        assert!(created, "should create fresh note");
        assert_ne!(note.0, new_id, "should be different id");

        let new_note = storage
            .get_note(new_id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(new_note.body, "fresh");
    }

    #[tokio::test]
    async fn daily_note_rolls_unfinished_tasks_forward_once() {
        let storage = test_storage().await;

        // Yesterday's note: one done, two open tasks.
        let (_y, _) = storage
            .find_or_create_daily_note(
                "2026-06-18",
                "# Mon\n- [x] shipped\n- [ ] write tests\n- [ ] review PR",
                "2026-06-18",
                true,
            )
            .await
            .expect("yesterday");

        // Today (rollup on): the two open tasks carry forward, the done one doesn't.
        let (today_id, created) = storage
            .find_or_create_daily_note("2026-06-19", "# Tue", "2026-06-19", true)
            .await
            .expect("today");
        assert!(created);
        let body = storage
            .get_note(today_id)
            .await
            .expect("get")
            .expect("exists")
            .body;
        assert_eq!(
            body,
            "# Tue\n\n## Carried over\n\n- [ ] write tests\n- [ ] review PR\n"
        );

        // Reopening today must not duplicate the rollup.
        let (reopen_id, created2) = storage
            .find_or_create_daily_note("2026-06-19", "# Tue", "2026-06-19", true)
            .await
            .expect("reopen");
        assert!(!created2);
        assert_eq!(reopen_id, today_id);
        let body2 = storage
            .get_note(reopen_id)
            .await
            .expect("get")
            .expect("exists")
            .body;
        assert_eq!(body2, body, "reopen must not re-roll");
    }

    #[tokio::test]
    async fn daily_note_rollup_disabled_keeps_template_only() {
        let storage = test_storage().await;
        storage
            .find_or_create_daily_note("2026-06-18", "- [ ] carry me", "2026-06-18", true)
            .await
            .expect("yesterday");

        let (id, _) = storage
            .find_or_create_daily_note("2026-06-19", "# Tue", "2026-06-19", false)
            .await
            .expect("today");
        let body = storage.get_note(id).await.expect("get").expect("exists").body;
        assert_eq!(body, "# Tue", "rollup off → template untouched");
    }

    #[tokio::test]
    async fn on_this_day_matches_prior_periods() {
        let storage = test_storage().await;

        // A note exactly a week before "today", and one a year before.
        storage
            .find_or_create_daily_note("2026-06-13", "week-ago", "2026-06-13", false)
            .await
            .expect("week");
        storage
            .find_or_create_daily_note("2025-06-20", "year-ago", "2025-06-20", false)
            .await
            .expect("year");
        // A non-matching daily note.
        storage
            .find_or_create_daily_note("2026-06-01", "noise", "2026-06-01", false)
            .await
            .expect("noise");

        // Today's note.
        let (today, _) = storage
            .find_or_create_daily_note("2026-06-20", "today", "2026-06-20", false)
            .await
            .expect("today");

        let hits = storage
            .on_this_day_notes(today, &[7, 30, 365])
            .await
            .expect("on this day");
        let titles: Vec<(u32, String)> =
            hits.iter().map(|(o, n)| (*o, n.title.clone())).collect();
        assert_eq!(
            titles,
            vec![(7, "2026-06-13".to_string()), (365, "2025-06-20".to_string())]
        );

        // A non-daily note (no daily_date) yields nothing.
        let plain = storage
            .create_note(CreateNoteInput { title: "plain".into(), body: "x".into() })
            .await
            .expect("plain");
        assert!(storage
            .on_this_day_notes(plain.id, &[7, 365])
            .await
            .expect("none")
            .is_empty());
    }

    #[tokio::test]
    async fn wikilink_index_backfills_and_backlinks() {
        let storage = test_storage().await;

        // A links to "Beta", which does not exist yet → dangling.
        let a = storage
            .create_note(CreateNoteInput {
                title: "Alpha".into(),
                body: "see [[Beta]] and [[beta]] again".into(),
            })
            .await
            .expect("create A");

        // Create the target later; its links should back-fill, so A now backlinks B.
        let b = storage
            .create_note(CreateNoteInput {
                title: "Beta".into(),
                body: "the beta note".into(),
            })
            .await
            .expect("create B");

        let back = storage.backlinks(b.id).await.expect("backlinks");
        assert_eq!(back.len(), 1, "duplicate [[Beta]]/[[beta]] collapse to one row");
        assert_eq!(back[0].id, a.id);

        // Editing A to drop the link clears the backlink.
        storage
            .update_note(UpdateNoteInput {
                id: a.id,
                title: "Alpha".into(),
                body: "no links now".into(),
            })
            .await
            .expect("update A");
        assert!(storage.backlinks(b.id).await.expect("backlinks").is_empty());
    }

    #[tokio::test]
    async fn purge_permanently_removes_trashed_note_and_tags() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Doomed".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");
        storage.add_tag_to_note(note.id, "gone").await.expect("tag");

        // Can't purge a live note.
        assert!(storage.purge_note(note.id).await.is_err());

        storage.soft_delete_note(note.id).await.expect("delete");
        storage.purge_note(note.id).await.expect("purge");

        assert!(storage.list_deleted_notes().await.expect("trash").is_empty());
        let tag_links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM note_tags WHERE note_id = ?")
            .bind(note.id.to_string())
            .fetch_one(&storage.pool)
            .await
            .expect("count");
        assert_eq!(tag_links, 0, "tag links removed");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE id = ?")
            .bind(note.id.to_string())
            .fetch_one(&storage.pool)
            .await
            .expect("count");
        assert_eq!(rows, 0, "note row gone");
    }

    #[tokio::test]
    async fn live_note_exists_with_content_matches_exactly() {
        let storage = test_storage().await;
        storage
            .create_note(CreateNoteInput {
                title: "Dupe".to_string(),
                body: "same body".to_string(),
            })
            .await
            .expect("create");

        assert!(storage
            .live_note_exists_with_content("Dupe", "same body")
            .await
            .expect("check"));
        assert!(!storage
            .live_note_exists_with_content("Dupe", "different")
            .await
            .expect("check"));
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_create_note_enqueues_embedding_without_polluting_sync_queue() {
        let storage = test_storage().await;

        let note = storage
            .create_note(CreateNoteInput {
                title: "AI Note".to_string(),
                body: "semantic body".to_string(),
            })
            .await
            .expect("create");

        assert_eq!(
            queued_embedding_hash(&storage, note.id).await.as_deref(),
            Some(note.content_hash.as_str())
        );

        let sync_entries = storage
            .get_sync_queue_entries(10)
            .await
            .expect("sync entries");
        assert_eq!(sync_entries.len(), 1);
        assert_eq!(sync_entries[0].entity_type, "note");
        assert_eq!(sync_entries[0].operation, "create");
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_update_note_replaces_stale_embedding_queue_entry() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Draft".to_string(),
                body: "old body".to_string(),
            })
            .await
            .expect("create");

        sqlx::query(
            "UPDATE embedding_queue SET attempts = 3, last_error = 'temporary failure' WHERE note_id = ?",
        )
        .bind(note.id.to_string())
        .execute(&storage.pool)
        .await
        .expect("dirty queue row");

        let updated = storage
            .update_note(UpdateNoteInput {
                id: note.id,
                title: "Draft".to_string(),
                body: "new body".to_string(),
            })
            .await
            .expect("update");

        assert_ne!(updated.content_hash, note.content_hash);
        assert_eq!(
            queued_embedding_hash(&storage, note.id).await.as_deref(),
            Some(updated.content_hash.as_str())
        );
        assert_eq!(embedding_queue_count(&storage, note.id).await, 1);

        let row = sqlx::query("SELECT attempts, last_error FROM embedding_queue WHERE note_id = ?")
            .bind(note.id.to_string())
            .fetch_one(&storage.pool)
            .await
            .expect("queue row");
        assert_eq!(row.get::<i32, _>("attempts"), 0);
        assert_eq!(row.get::<Option<String>, _>("last_error"), None);
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_update_note_skips_embedding_when_hash_already_embedded() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Already Embedded".to_string(),
                body: "same body".to_string(),
            })
            .await
            .expect("create");
        mark_note_embedded(&storage, note.id, &note.content_hash).await;

        sqlx::query(
            r#"
            INSERT INTO embedding_queue (note_id, content_hash, enqueued_at)
            VALUES (?, 'stale-hash', ?)
            ON CONFLICT(note_id) DO UPDATE SET
                content_hash = excluded.content_hash,
                enqueued_at = excluded.enqueued_at
            "#,
        )
        .bind(note.id.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(&storage.pool)
        .await
        .expect("seed stale queue row");

        let updated = storage
            .update_note(UpdateNoteInput {
                id: note.id,
                title: note.title.clone(),
                body: note.body.clone(),
            })
            .await
            .expect("update with same content");

        assert_eq!(updated.content_hash, note.content_hash);
        assert_eq!(queued_embedding_hash(&storage, note.id).await, None);
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_soft_delete_removes_embedding_queue_and_bookkeeping() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Delete AI State".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");
        mark_note_embedded(&storage, note.id, &note.content_hash).await;

        storage
            .soft_delete_note(note.id)
            .await
            .expect("soft delete");

        assert_eq!(queued_embedding_hash(&storage, note.id).await, None);
        let embedding_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?")
                .bind(note.id.to_string())
                .fetch_one(&storage.pool)
                .await
                .expect("note embedding count");
        assert_eq!(embedding_rows, 0);
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_remote_upsert_enqueues_embedding_without_sync_echo() {
        let storage = test_storage().await;
        let note_id = Uuid::new_v4();
        let now = Utc::now();

        storage
            .upsert_note_from_remote(note_id, "Remote", "pulled body", now, now)
            .await
            .expect("remote upsert");

        let note = storage
            .get_note(note_id)
            .await
            .expect("get note")
            .expect("note exists");
        assert_eq!(
            queued_embedding_hash(&storage, note_id).await.as_deref(),
            Some(note.content_hash.as_str())
        );
        assert!(
            storage
                .get_sync_queue_entries(10)
                .await
                .expect("sync entries")
                .is_empty(),
            "remote pulls must not enqueue outbound sync work"
        );
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn sqlite_vec_is_ready_and_stores_vectors() {
        // Use the real connect path (not the fast not-ready test harness) so we
        // exercise the bundled-extension registration end to end.
        let storage = SqliteStorage::connect_with_ai(Path::new(":memory:"), true)
            .await
            .expect("connect with ai");
        // The whole point of AI-1a′: the bundled extension makes this reachable.
        assert!(
            storage.ai_available(),
            "sqlite-vec should be statically available; status: {:?}",
            storage.ai_sqlite_status()
        );

        let note = storage
            .create_note(CreateNoteInput {
                title: "Vector".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");
        let vector = vec![0.1f32; 384];
        storage
            .store_note_embedding(note.id, "test-model", 384, &note.content_hash, &vector)
            .await
            .expect("store embedding");

        let vec_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vec_notes WHERE note_id = ?")
            .bind(note.id.to_string())
            .fetch_one(&storage.pool)
            .await
            .expect("vec_notes count");
        assert_eq!(vec_rows, 1, "vector should be persisted in vec_notes");
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn embedding_model_changed_detects_id_and_dimension_changes() {
        let storage = test_storage().await;
        // Migrations seed the index model as 'default-local' / 384 dims.
        assert!(
            !storage
                .embedding_model_changed("default-local", 384)
                .await
                .expect("same model"),
            "matching id+dims is not a change"
        );
        assert!(
            storage
                .embedding_model_changed("hashed-bow-v1", 384)
                .await
                .expect("different id"),
            "a different model id is a change"
        );
        assert!(
            storage
                .embedding_model_changed("default-local", 512)
                .await
                .expect("different dims"),
            "a different vector width is a change"
        );
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn reindex_embeddings_resets_index_and_requeues_live_notes() {
        let storage = SqliteStorage::connect_with_ai(Path::new(":memory:"), true)
            .await
            .expect("connect with ai");
        assert!(storage.ai_available(), "sqlite-vec must be available");

        // Two live notes, both embedded, plus one soft-deleted note.
        let mut live = Vec::new();
        for title in ["alpha", "beta"] {
            let note = storage
                .create_note(CreateNoteInput {
                    title: title.to_string(),
                    body: "body".to_string(),
                })
                .await
                .expect("create");
            storage
                .store_note_embedding(note.id, "old-model", 384, &note.content_hash, &vec![0.1; 384])
                .await
                .expect("embed");
            live.push(note);
        }
        let gone = storage
            .create_note(CreateNoteInput {
                title: "gone".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");
        storage.soft_delete_note(gone.id).await.expect("delete");

        // Sanity: vectors and bookkeeping exist before the reindex.
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM note_embeddings")
            .fetch_one(&storage.pool)
            .await
            .expect("count");
        assert_eq!(before, 2);

        let enqueued = storage
            .reindex_embeddings("new-model", 384)
            .await
            .expect("reindex");

        // Only the two live notes are re-enqueued; the deleted one is skipped.
        assert_eq!(enqueued, 2, "live notes re-enqueued");
        for note in &live {
            assert_eq!(queued_embedding_hash(&storage, note.id).await.as_deref(), Some(note.content_hash.as_str()));
        }
        assert_eq!(embedding_queue_count(&storage, gone.id).await, 0);

        // Old vectors and bookkeeping are wiped.
        let after_embeddings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM note_embeddings")
            .fetch_one(&storage.pool)
            .await
            .expect("count");
        let after_vectors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vec_notes")
            .fetch_one(&storage.pool)
            .await
            .expect("count");
        assert_eq!(after_embeddings, 0, "stale embeddings cleared");
        assert_eq!(after_vectors, 0, "stale vectors cleared");

        // Metadata now records the new model, so no further change is detected.
        assert_eq!(
            storage.get_metadata("embedding_model_id").await.expect("meta").as_deref(),
            Some("new-model")
        );
        assert!(!storage
            .embedding_model_changed("new-model", 384)
            .await
            .expect("settled"));
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn related_notes_and_tag_suggestions_use_stored_vectors() {
        let storage = SqliteStorage::connect_with_ai(Path::new(":memory:"), true)
            .await
            .expect("connect with ai");
        assert!(storage.ai_available(), "sqlite-vec must be available");

        // Helper: create a note and embed it with a given 384-dim vector.
        async fn seed(storage: &SqliteStorage, title: &str, dim0: f32, dim1: f32) -> Note {
            let note = storage
                .create_note(CreateNoteInput {
                    title: title.to_string(),
                    body: "body".to_string(),
                })
                .await
                .expect("create");
            let mut vector = vec![0.0f32; 384];
            vector[0] = dim0;
            vector[1] = dim1;
            storage
                .store_note_embedding(note.id, "test-model", 384, &note.content_hash, &vector)
                .await
                .expect("store embedding");
            note
        }

        // `anchor` is closest to `near` and far from `far`.
        let anchor = seed(&storage, "anchor", 1.0, 0.0).await;
        let near = seed(&storage, "near", 0.95, 0.05).await;
        let far = seed(&storage, "far", -1.0, 0.0).await;

        // Tag the neighbors so suggestions have something to draw from.
        storage.add_tag_to_note(near.id, "rust").await.expect("tag");
        storage.add_tag_to_note(far.id, "rust").await.expect("tag");
        storage.add_tag_to_note(near.id, "search").await.expect("tag");
        // anchor already carries one of the candidate tags — it must be excluded.
        storage.add_tag_to_note(anchor.id, "rust").await.expect("tag");

        // Related notes: both others come back, nearest first.
        let related = storage.related_notes(anchor.id, 5).await.expect("related");
        let ids: Vec<Uuid> = related.iter().map(|s| s.id).collect();
        assert!(!ids.contains(&anchor.id), "must exclude the note itself");
        assert_eq!(ids.first(), Some(&near.id), "closest neighbor ranks first");
        assert!(ids.contains(&far.id), "all neighbors returned within k");

        // Tag suggestions: "rust" is already on anchor (excluded); "search"
        // (only on the nearest neighbor) should surface.
        let suggested = storage
            .suggest_tags_for_note(anchor.id, 5, 5)
            .await
            .expect("suggest");
        assert!(
            suggested.contains(&"search".to_string()),
            "neighbor-only tag should be suggested, got {suggested:?}"
        );
        assert!(
            !suggested.contains(&"rust".to_string()),
            "tags already on the note must be excluded, got {suggested:?}"
        );

        // A note with no stored vector yields nothing rather than erroring.
        let orphan = storage
            .create_note(CreateNoteInput {
                title: "orphan".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");
        assert!(storage
            .related_notes(orphan.id, 5)
            .await
            .expect("related orphan")
            .is_empty());
        assert!(storage
            .suggest_tags_for_note(orphan.id, 5, 5)
            .await
            .expect("suggest orphan")
            .is_empty());
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_fetch_embedding_batch_returns_queued_notes() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Indexable".to_string(),
                body: "searchable body".to_string(),
            })
            .await
            .expect("create");

        let batch = storage.fetch_embedding_batch(10).await.expect("batch");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].note_id, note.id);
        assert_eq!(batch[0].content_hash, note.content_hash);
        assert!(batch[0].text.contains("Indexable"));
        assert!(batch[0].text.contains("searchable body"));
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_store_note_embedding_writes_bookkeeping_and_dequeues() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Store".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");

        storage
            .store_note_embedding(note.id, "test-model", 384, &note.content_hash, &[0.0, 1.0])
            .await
            .expect("store");

        assert_eq!(embedding_queue_count(&storage, note.id).await, 0);
        let (model, dims, hash): (String, i64, String) = sqlx::query_as(
            "SELECT model_id, dimensions, content_hash FROM note_embeddings WHERE note_id = ?",
        )
        .bind(note.id.to_string())
        .fetch_one(&storage.pool)
        .await
        .expect("embedding row");
        assert_eq!(model, "test-model");
        assert_eq!(dims, 384);
        assert_eq!(hash, note.content_hash);
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_store_note_embedding_keeps_queue_when_hash_superseded() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Superseded".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");

        // Embed an older hash than the one queued; the queued row must survive
        // so the newer content gets re-embedded.
        storage
            .store_note_embedding(note.id, "test-model", 384, "stale-hash", &[0.0, 1.0])
            .await
            .expect("store");

        assert_eq!(embedding_queue_count(&storage, note.id).await, 1);
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_mark_embedding_failed_records_attempt() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Fail".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");

        let retry_after = Utc::now() + chrono::Duration::seconds(5);
        storage
            .mark_embedding_failed(note.id, "boom", retry_after)
            .await
            .expect("mark failed");

        let (attempts, last_error, next_attempt_at): (i64, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT attempts, last_error, next_attempt_at FROM embedding_queue WHERE note_id = ?",
            )
            .bind(note.id.to_string())
            .fetch_one(&storage.pool)
            .await
            .expect("queue row");
        assert_eq!(attempts, 1);
        assert_eq!(last_error.as_deref(), Some("boom"));
        assert!(next_attempt_at.is_some(), "backoff window recorded");
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn fetch_embedding_batch_skips_jobs_in_backoff() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Backoff".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("create");

        // A future backoff window hides the job from the batch.
        storage
            .mark_embedding_failed(note.id, "boom", Utc::now() + chrono::Duration::seconds(300))
            .await
            .expect("mark failed");
        assert!(
            storage.fetch_embedding_batch(10).await.expect("batch").is_empty(),
            "job in backoff must not be returned"
        );

        // A window in the past makes it due again, carrying the attempt count.
        storage
            .mark_embedding_failed(note.id, "boom", Utc::now() - chrono::Duration::seconds(1))
            .await
            .expect("mark failed");
        let batch = storage.fetch_embedding_batch(10).await.expect("batch");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].attempts, 2, "attempts surfaced to the worker");
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_count_pending_embeddings_counts_queue() {
        let storage = test_storage().await;
        for i in 0..2 {
            storage
                .create_note(CreateNoteInput {
                    title: format!("Note {i}"),
                    body: "body".to_string(),
                })
                .await
                .expect("create");
        }
        assert_eq!(storage.count_pending_embeddings().await.expect("count"), 2);
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_get_metadata_reads_seeded_and_missing_keys() {
        let storage = test_storage().await;
        assert_eq!(
            storage
                .get_metadata("embedding_model_id")
                .await
                .expect("get")
                .as_deref(),
            Some("default-local")
        );
        assert!(storage
            .get_metadata("nope_not_a_key")
            .await
            .expect("get")
            .is_none());
    }

    #[tokio::test]
    async fn test_connect_twice_is_idempotent() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("jot_test_{}.db", Uuid::new_v4()));
        // First connect runs migrations.
        let s1 = SqliteStorage::connect(&path).await.expect("first connect");
        drop(s1);
        // Second connect must not fail re-running migrations.
        let s2 = SqliteStorage::connect(&path).await.expect("second connect");
        drop(s2);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_ai_foundation_migration_creates_bookkeeping_schema() {
        let storage = test_storage().await;

        let note_embeddings_exists = storage
            .table_exists("note_embeddings")
            .await
            .expect("note_embeddings exists check");
        let embedding_queue_exists = storage
            .table_exists("embedding_queue")
            .await
            .expect("embedding_queue exists check");

        assert!(note_embeddings_exists);
        assert!(embedding_queue_exists);

        let keys: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT key
            FROM local_metadata
            WHERE key IN (
                'embedding_model_id',
                'embedding_dimensions',
                'ai_index_schema_version'
            )
            ORDER BY key
            "#,
        )
        .fetch_all(&storage.pool)
        .await
        .expect("metadata keys");

        assert_eq!(
            keys,
            vec![
                "ai_index_schema_version".to_string(),
                "embedding_dimensions".to_string(),
                "embedding_model_id".to_string(),
            ]
        );
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_vec_notes_migration_skips_without_sqlite_vec() {
        let storage = test_storage().await;

        assert!(
            !storage
                .table_exists("vec_notes")
                .await
                .expect("vec_notes exists check"),
            "vec_notes should not be created when sqlite-vec was not loaded"
        );

        let applied: Option<i64> =
            sqlx::query_scalar("SELECT version FROM schema_migrations WHERE version = 7")
                .fetch_optional(&storage.pool)
                .await
                .expect("migration lookup");

        assert_eq!(
            applied, None,
            "skipped sqlite-vec migration should remain unapplied"
        );
    }

    #[tokio::test]
    async fn test_connect_with_ai_disabled_skips_ai_schema() {
        let path = std::env::temp_dir().join(format!("jot_no_ai_{}.db", Uuid::new_v4()));

        let storage = SqliteStorage::connect_with_ai(&path, false)
            .await
            .expect("connect with AI disabled");

        assert_eq!(
            storage.ai_sqlite_status(),
            &AiSqliteStatus::DisabledByConfig
        );
        assert!(!storage.ai_available());
        assert!(!storage
            .table_exists("note_embeddings")
            .await
            .expect("note_embeddings exists check"));

        drop(storage);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_connect_with_ai_falls_back_if_sqlite_vec_is_missing() {
        let path = std::env::temp_dir().join(format!("jot_ai_fallback_{}.db", Uuid::new_v4()));

        let storage = SqliteStorage::connect_with_ai(&path, true)
            .await
            .expect("connect should not fail when sqlite-vec is unavailable");

        match storage.ai_sqlite_status() {
            AiSqliteStatus::Ready { .. } => {
                assert!(storage.ai_available());
            }
            AiSqliteStatus::VectorExtensionUnavailable { error } => {
                assert!(!error.is_empty());
                assert!(!storage.ai_available());
            }
            other => panic!("unexpected AI SQLite status: {:?}", other),
        }

        assert!(
            storage
                .table_exists("note_embeddings")
                .await
                .expect("note_embeddings exists check"),
            "AI bookkeeping schema should still be present when vec loading fails"
        );

        drop(storage);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_migrates_legacy_untracked_db() {
        // Reproduce a database created before migration tracking existed:
        // migrations 1–3 applied directly, no schema_migrations table, and the
        // content_hash column (migration 4) missing.
        let path = std::env::temp_dir().join(format!("jot_legacy_{}.db", Uuid::new_v4()));
        {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(&path)
                        .create_if_missing(true),
                )
                .await
                .expect("open legacy db");
            for sql in [
                include_str!("migrations/20250615000001_initial.sql"),
                include_str!("migrations/20250615000002_sync_queue.sql"),
                include_str!("migrations/20250615000003_conflicts.sql"),
            ] {
                sqlx::query(sql)
                    .execute(&pool)
                    .await
                    .expect("legacy migrate");
            }
            pool.close().await;
        }

        // Connecting through the real runner must NOT fail re-running the
        // non-idempotent ALTER in migration 3, and must apply migration 4.
        let storage = SqliteStorage::connect(&path)
            .await
            .expect("legacy db should migrate cleanly");

        // content_hash now exists → get_note works.
        let note = storage
            .create_note(CreateNoteInput {
                title: "Legacy".into(),
                body: "body".into(),
            })
            .await
            .expect("create");
        assert!(storage.get_note(note.id).await.expect("get_note").is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_get_note_returns_full_note() {
        let storage = test_storage().await;
        let note = storage
            .create_note(CreateNoteInput {
                title: "Editor Note".to_string(),
                body: "body text".to_string(),
            })
            .await
            .expect("create");
        let fetched = storage
            .get_note(note.id)
            .await
            .expect("get_note should succeed");
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().body, "body text");
    }

    #[tokio::test]
    async fn test_soft_delete_note() {
        let storage = test_storage().await;

        let note = storage
            .create_note(CreateNoteInput {
                title: "To Delete".to_string(),
                body: String::new(),
            })
            .await
            .expect("Failed to create note");

        storage
            .soft_delete_note(note.id)
            .await
            .expect("Failed to delete note");

        let notes = storage.list_notes().await.expect("Failed to list notes");
        assert!(notes.is_empty(), "Deleted note should not appear in list");
    }

    #[tokio::test]
    async fn test_search_notes() {
        let storage = test_storage().await;

        storage
            .create_note(CreateNoteInput {
                title: "Rust Programming".to_string(),
                body: "Learning about ownership and borrowing.".to_string(),
            })
            .await
            .expect("Failed to create note");

        storage
            .create_note(CreateNoteInput {
                title: "Grocery List".to_string(),
                body: "Milk, eggs, bread.".to_string(),
            })
            .await
            .expect("Failed to create note");

        let results = storage
            .search_notes("Rust")
            .await
            .expect("Failed to search");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust Programming");

        let results = storage
            .search_notes("eggs")
            .await
            .expect("Failed to search");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Grocery List");
    }

    #[tokio::test]
    async fn test_tags() {
        let storage = test_storage().await;

        let note = storage
            .create_note(CreateNoteInput {
                title: "Tagged Note".to_string(),
                body: String::new(),
            })
            .await
            .expect("Failed to create note");

        storage
            .add_tag_to_note(note.id, "rust")
            .await
            .expect("Failed to add tag");

        storage
            .add_tag_to_note(note.id, "testing")
            .await
            .expect("Failed to add tag");

        let tags = storage
            .get_tags_for_note(note.id)
            .await
            .expect("Failed to get tags");
        assert_eq!(tags.len(), 2);
        assert!(tags.contains(&"rust".to_string()));
        assert!(tags.contains(&"testing".to_string()));

        // List all tags
        let all_tags = storage.list_tags().await.expect("Failed to list tags");
        assert_eq!(all_tags.len(), 2);
    }

    #[tokio::test]
    async fn test_get_or_create_tag() {
        let storage = test_storage().await;

        let tag = storage
            .get_or_create_tag("important")
            .await
            .expect("Failed to create tag");
        assert_eq!(tag.name, "important");

        // Getting the same tag name should return the existing tag
        let same = storage
            .get_or_create_tag("important")
            .await
            .expect("Failed to get existing tag");
        assert_eq!(same.id, tag.id);
        assert_eq!(same.name, "important");
    }

    #[tokio::test]
    async fn test_sync_queue_backoff() {
        let storage = test_storage().await;

        // Creating a note enqueues one "create" entry, due immediately.
        storage
            .create_note(CreateNoteInput {
                title: "Backoff".to_string(),
                body: String::new(),
            })
            .await
            .expect("create");

        let due = storage
            .get_due_sync_queue_entries(10)
            .await
            .expect("due entries");
        assert_eq!(due.len(), 1, "fresh entry should be due");
        let entry_id = due[0].id;

        // Mark it failed with a future retry time — it should drop out of "due".
        let future = Utc::now() + chrono::Duration::seconds(300);
        storage
            .mark_sync_queue_error(entry_id, "boom", future)
            .await
            .expect("mark error");
        let due = storage
            .get_due_sync_queue_entries(10)
            .await
            .expect("due entries");
        assert!(due.is_empty(), "backed-off entry should not be due yet");

        // It is still present in the full queue (just deferred).
        assert_eq!(
            storage.get_sync_queue_entries(10).await.unwrap().len(),
            1,
            "entry should still be queued"
        );

        // A manual sync clears backoff, making it due again.
        storage.reset_sync_backoff().await.expect("reset");
        let due = storage
            .get_due_sync_queue_entries(10)
            .await
            .expect("due entries");
        assert_eq!(due.len(), 1, "reset should make the entry due again");
        assert_eq!(due[0].attempts, 1, "attempt count should have incremented");
    }

    #[tokio::test]
    async fn test_sync_queue_entries() {
        let storage = test_storage().await;

        // Creating a note should enqueue a "create" operation
        let note = storage
            .create_note(CreateNoteInput {
                title: "Sync Test".to_string(),
                body: "Check queue entries.".to_string(),
            })
            .await
            .expect("Failed to create note");

        // Update should enqueue an "update"
        storage
            .update_note(UpdateNoteInput {
                id: note.id,
                title: "Sync Test Updated".to_string(),
                body: note.body.clone(),
            })
            .await
            .expect("Failed to update note");

        // Adding a tag should enqueue a "tag_add"
        storage
            .add_tag_to_note(note.id, "sync-tag")
            .await
            .expect("Failed to add tag");

        // Removing a tag should enqueue a "tag_remove"
        storage
            .remove_tag_by_name(note.id, "sync-tag")
            .await
            .expect("Failed to remove tag");

        // Soft-delete should enqueue a "delete"
        storage
            .soft_delete_note(note.id)
            .await
            .expect("Failed to delete note");

        // Verify queue has 5 entries in order
        let rows = sqlx::query(
            "SELECT operation, entity_type, entity_id, payload_json FROM sync_queue ORDER BY id",
        )
        .fetch_all(&storage.pool)
        .await
        .expect("Failed to query sync_queue");

        assert_eq!(rows.len(), 5, "Expected 5 sync_queue entries");

        assert_eq!(rows[0].get::<&str, _>("operation"), "create");
        assert_eq!(rows[0].get::<&str, _>("entity_type"), "note");

        assert_eq!(rows[1].get::<&str, _>("operation"), "update");
        assert_eq!(rows[1].get::<&str, _>("entity_type"), "note");

        assert_eq!(rows[2].get::<&str, _>("operation"), "tag_add");
        assert_eq!(rows[2].get::<&str, _>("entity_type"), "note_tag");

        assert_eq!(rows[3].get::<&str, _>("operation"), "tag_remove");
        assert_eq!(rows[3].get::<&str, _>("entity_type"), "note_tag");

        assert_eq!(rows[4].get::<&str, _>("operation"), "delete");
        assert_eq!(rows[4].get::<&str, _>("entity_type"), "note");

        // Tag sync payloads must carry the tag_id so the push handler can link
        // the tag remotely (regression guard for the tag-sync mismatch fix).
        for idx in [2usize, 3usize] {
            let payload: String = rows[idx].get("payload_json");
            let value: serde_json::Value =
                serde_json::from_str(&payload).expect("payload should be valid JSON");
            assert!(
                value.get("tag_id").and_then(|v| v.as_str()).is_some(),
                "tag sync entry {} should include a tag_id",
                idx
            );
        }
    }

    #[cfg(feature = "ai")]
    #[tokio::test]
    async fn test_enqueue_all_stale_embeddings_backfills_unembedded_notes() {
        let storage = test_storage().await;

        // Insert notes directly (bypass create_note which already enqueues
        // embeddings) to simulate notes created before AI was enabled.
        let now = Utc::now().to_rfc3339();

        // n1 — already embedded with matching hash.
        let n1_id = Uuid::new_v4();
        let n1_hash = "hash-one".to_string();
        sqlx::query(
            "INSERT INTO notes (id, title, body, created_at, updated_at, content_hash) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(n1_id.to_string())
        .bind("Already Embedded")
        .bind("done")
        .bind(&now)
        .bind(&now)
        .bind(&n1_hash)
        .execute(&storage.pool)
        .await
        .expect("insert n1");
        mark_note_embedded(&storage, n1_id, &n1_hash).await;

        // n2 — embedded with an old hash (stale content).
        let n2_id = Uuid::new_v4();
        let n2_hash = "hash-two-fresh".to_string();
        sqlx::query(
            "INSERT INTO notes (id, title, body, created_at, updated_at, content_hash) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(n2_id.to_string())
        .bind("Stale Content")
        .bind("changed body")
        .bind(&now)
        .bind(&now)
        .bind(&n2_hash)
        .execute(&storage.pool)
        .await
        .expect("insert n2");
        mark_note_embedded(&storage, n2_id, "hash-two-old").await;

        // n3 — never embedded.
        let n3_id = Uuid::new_v4();
        let n3_hash = "hash-three".to_string();
        sqlx::query(
            "INSERT INTO notes (id, title, body, created_at, updated_at, content_hash) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(n3_id.to_string())
        .bind("Never Embedded")
        .bind("fresh")
        .bind(&now)
        .bind(&now)
        .bind(&n3_hash)
        .execute(&storage.pool)
        .await
        .expect("insert n3");

        // Deleted note — must be skipped.
        let deleted_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO notes (id, title, body, created_at, updated_at, content_hash, deleted_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(deleted_id.to_string())
        .bind("Deleted")
        .bind("gone")
        .bind(&now)
        .bind(&now)
        .bind("hash-deleted")
        .bind(&now)
        .execute(&storage.pool)
        .await
        .expect("insert deleted");

        let inserted = storage
            .enqueue_all_stale_embeddings()
            .await
            .expect("backfill");
        // n2 (stale hash) + n3 (never embedded) = 2
        assert_eq!(inserted, 2);

        // Idempotent — re-running should insert 0.
        let again = storage
            .enqueue_all_stale_embeddings()
            .await
            .expect("backfill again");
        assert_eq!(again, 0);

        // Verify the two enqueued notes are in the queue.
        assert_eq!(
            queued_embedding_hash(&storage, n2_id).await.as_deref(),
            Some(n2_hash.as_str()),
            "n2 should be queued with current content_hash"
        );
        assert_eq!(
            queued_embedding_hash(&storage, n3_id).await.as_deref(),
            Some(n3_hash.as_str()),
            "n3 should be queued with current content_hash"
        );

        // n1 is already embedded with the right hash → not queued.
        assert!(
            queued_embedding_hash(&storage, n1_id).await.is_none(),
            "n1 should not be queued (already embedded with matching hash)"
        );
    }
}
