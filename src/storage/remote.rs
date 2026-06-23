use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::models::note::Note;

/// The remote PostgreSQL storage backend.
#[derive(Debug, Clone)]
pub struct PostgresStorage {
    pool: PgPool,
    device_id: Uuid,
}

impl PostgresStorage {
    /// Get the device ID used by this storage instance.
    pub fn device_id(&self) -> Uuid {
        self.device_id
    }

    /// Access the underlying pool for assertions in integration tests.
    #[cfg(test)]
    pub fn pool_for_tests(&self) -> &PgPool {
        &self.pool
    }

    /// Connect to PostgreSQL and run migrations.
    pub async fn connect(database_url: &str, device_id: Uuid) -> Result<Self> {
        let options: PgConnectOptions = database_url
            .parse()
            .context("Failed to parse PostgreSQL database URL")?;

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .context("Failed to connect to PostgreSQL database")?;

        let storage = Self { pool, device_id };
        storage.run_migrations().await?;

        Ok(storage)
    }

    /// Run PostgreSQL migrations using sqlx's built-in migration runner.
    async fn run_migrations(&self) -> Result<()> {
        // Use sqlx migrations from the migrations/postgres directory
        sqlx::migrate!("migrations/postgres")
            .run(&self.pool)
            .await
            .context("Failed to run PostgreSQL migrations")?;

        tracing::info!("PostgreSQL migrations applied successfully");
        Ok(())
    }

    /// Register (or update) this device with the remote backend.
    pub async fn register_device(&self, device_name: &str) -> Result<()> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO devices (id, name, created_at, last_seen)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (id) DO UPDATE SET
                last_seen = EXCLUDED.last_seen
            "#,
        )
        .bind(self.device_id)
        .bind(device_name)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .context("Failed to register device")?;

        // Initialize sync cursor if not exists
        sqlx::query(
            r#"
            INSERT INTO sync_cursors (device_id, last_event_id, updated_at)
            VALUES ($1, 0, $2)
            ON CONFLICT (device_id) DO NOTHING
            "#,
        )
        .bind(self.device_id)
        .bind(now)
        .execute(&self.pool)
        .await?;

        tracing::info!("Device registered: {} ({})", device_name, self.device_id);
        Ok(())
    }

    /// Get the last seen event ID for this device.
    pub async fn get_last_event_id(&self) -> Result<i64> {
        let row = sqlx::query("SELECT last_event_id FROM sync_cursors WHERE device_id = $1")
            .bind(self.device_id)
            .fetch_optional(&self.pool)
            .await?
            .with_context(|| "Sync cursor not found for device")?;

        Ok(row.get::<i64, _>("last_event_id"))
    }

    /// Update the sync cursor for this device.
    pub async fn update_cursor(&self, last_event_id: i64) -> Result<()> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO sync_cursors (device_id, last_event_id, updated_at)
            VALUES ($1, $2, $3)
            ON CONFLICT (device_id) DO UPDATE SET
                last_event_id = EXCLUDED.last_event_id,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(self.device_id)
        .bind(last_event_id)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Fetch sync events after a given event ID.
    pub async fn fetch_events_after(&self, after_id: i64, limit: i64) -> Result<Vec<SyncEvent>> {
        let rows = sqlx::query(
            r#"
            SELECT id, entity_type, entity_id, operation, payload_json, device_id, created_at
            FROM sync_events
            WHERE id > $1
            ORDER BY id ASC
            LIMIT $2
            "#,
        )
        .bind(after_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(SyncEvent {
                id: row.get("id"),
                entity_type: row.get("entity_type"),
                entity_id: row.get("entity_id"),
                operation: row.get("operation"),
                payload_json: row.get("payload_json"),
                device_id: row.get("device_id"),
                created_at: row.get("created_at"),
            });
        }

        Ok(events)
    }

    /// Write a sync event to the remote event log.
    async fn write_event(
        &self,
        entity_type: &str,
        entity_id: Uuid,
        operation: &str,
        payload: &serde_json::Value,
    ) -> Result<i64> {
        let row = sqlx::query(
            r#"
            INSERT INTO sync_events (entity_type, entity_id, operation, payload_json, device_id)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id
            "#,
        )
        .bind(entity_type)
        .bind(entity_id)
        .bind(operation)
        .bind(payload)
        .bind(self.device_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get("id"))
    }

    /// Upsert a note remotely if the version matches.
    /// Returns Ok(true) if the note was updated, Ok(false) if version mismatch (conflict).
    pub async fn upsert_note(&self, note: &Note, expected_version: Option<i32>) -> Result<bool> {
        // Try update first with version check
        if let Some(ver) = expected_version {
            let result = sqlx::query(
                r#"
                UPDATE notes
                SET title = $1, body = $2, updated_at = now(),
                    version = version + 1, last_modified_by_device_id = $3,
                    content_hash = $4
                WHERE id = $5 AND version = $6 AND deleted_at IS NULL
                "#,
            )
            .bind(&note.title)
            .bind(&note.body)
            .bind(self.device_id)
            .bind(&note.content_hash)
            .bind(note.id)
            .bind(ver)
            .execute(&self.pool)
            .await?;

            if result.rows_affected() == 0 {
                return Ok(false); // conflict
            }
        } else {
            // Insert or update without version check (new note)
            sqlx::query(
                r#"
                INSERT INTO notes (id, title, body, created_at, updated_at, version, content_hash, last_modified_by_device_id)
                VALUES ($1, $2, $3, $4, $5, 1, $6, $7)
                ON CONFLICT (id) DO UPDATE SET
                    title = EXCLUDED.title,
                    body = EXCLUDED.body,
                    updated_at = EXCLUDED.updated_at,
                    version = notes.version + 1,
                    content_hash = EXCLUDED.content_hash,
                    last_modified_by_device_id = EXCLUDED.last_modified_by_device_id
                "#,
            )
            .bind(note.id)
            .bind(&note.title)
            .bind(&note.body)
            .bind(note.created_at)
            .bind(note.updated_at)
            .bind(&note.content_hash)
            .bind(self.device_id)
            .execute(&self.pool)
            .await?;
        }

        // Write sync event
        let payload = serde_json::json!({
            "title": note.title,
            "body": note.body,
            "content_hash": note.content_hash,
        });
        self.write_event("note", note.id, "update", &payload)
            .await?;

        Ok(true)
    }

    /// Delete a note remotely.
    pub async fn delete_note(&self, note_id: Uuid) -> Result<()> {
        let now = Utc::now();
        sqlx::query("UPDATE notes SET deleted_at = $1 WHERE id = $2 AND deleted_at IS NULL")
            .bind(now)
            .bind(note_id)
            .execute(&self.pool)
            .await?;

        self.write_event("note", note_id, "delete", &serde_json::json!({}))
            .await?;

        Ok(())
    }

    /// Fetch a note from the remote backend.
    pub async fn fetch_note(&self, note_id: Uuid) -> Result<Option<RemoteNote>> {
        let row = sqlx::query(
            r#"
            SELECT id, title, body, created_at, updated_at, version, content_hash, last_modified_by_device_id
            FROM notes
            WHERE id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(note_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => Ok(Some(RemoteNote {
                id: r.get("id"),
                title: r.get("title"),
                body: r.get("body"),
                created_at: r.get("created_at"),
                updated_at: r.get("updated_at"),
                version: r.get("version"),
                content_hash: r.get("content_hash"),
                last_modified_by_device_id: r.get("last_modified_by_device_id"),
            })),
            None => Ok(None),
        }
    }

    /// Upsert a tag remotely.
    pub async fn upsert_tag(&self, tag_id: Uuid, name: &str) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO tags (id, name)
            VALUES ($1, $2)
            ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name
            "#,
        )
        .bind(tag_id)
        .bind(name)
        .execute(&self.pool)
        .await?;

        self.write_event("tag", tag_id, "update", &serde_json::json!({"name": name}))
            .await?;

        Ok(())
    }

    /// Add a note-tag relationship remotely and record a sync event so other
    /// devices learn about the link. The tag name travels in the payload so the
    /// puller can resolve the tag locally (tag ids are per-device).
    pub async fn add_note_tag(&self, note_id: Uuid, tag_id: Uuid, tag_name: &str) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO note_tags (note_id, tag_id)
            VALUES ($1, $2)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(note_id)
        .bind(tag_id)
        .execute(&self.pool)
        .await?;

        self.write_event(
            "note_tag",
            note_id,
            "tag_add",
            &serde_json::json!({ "tag_id": tag_id, "name": tag_name }),
        )
        .await?;

        Ok(())
    }

    /// Remove a note-tag relationship remotely and record a sync event.
    pub async fn remove_note_tag(&self, note_id: Uuid, tag_id: Uuid, tag_name: &str) -> Result<()> {
        sqlx::query("DELETE FROM note_tags WHERE note_id = $1 AND tag_id = $2")
            .bind(note_id)
            .bind(tag_id)
            .execute(&self.pool)
            .await?;

        self.write_event(
            "note_tag",
            note_id,
            "tag_remove",
            &serde_json::json!({ "tag_id": tag_id, "name": tag_name }),
        )
        .await?;

        Ok(())
    }
}

/// A sync event pulled from the remote event log.
// Full mapping of a remote sync-event row; some columns (e.g. created_at) are
// carried for completeness but not consumed by the sync logic yet.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SyncEvent {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: Uuid,
    pub operation: String,
    pub payload_json: serde_json::Value,
    pub device_id: Uuid,
    pub created_at: chrono::DateTime<Utc>,
}

/// A note as stored in the remote database. Full row mapping; some columns
/// (`version`, `last_modified_by_device_id`) aren't read on the local side yet.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RemoteNote {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
    pub version: i32,
    pub content_hash: String,
    pub last_modified_by_device_id: Uuid,
}
