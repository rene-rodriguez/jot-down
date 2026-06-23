-- Sync queue for tracking mutations that need to be pushed to PostgreSQL.
CREATE TABLE IF NOT EXISTS sync_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    operation TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sync_queue_created_at ON sync_queue(created_at);
CREATE INDEX IF NOT EXISTS idx_sync_queue_entity ON sync_queue(entity_type, entity_id);

-- Local metadata key-value store (device_id, schema_version, etc.)
CREATE TABLE IF NOT EXISTS local_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Seed schema version
INSERT OR IGNORE INTO local_metadata (key, value) VALUES ('schema_version', '1');
