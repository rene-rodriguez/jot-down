-- Local-only AI embedding bookkeeping. These tables are derived state and are
-- intentionally not represented in the sync queue or remote PostgreSQL schema.
CREATE TABLE IF NOT EXISTS note_embeddings (
    note_id TEXT PRIMARY KEY NOT NULL,
    model_id TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    embedded_at TEXT NOT NULL,
    FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_note_embeddings_hash ON note_embeddings(content_hash);

CREATE TABLE IF NOT EXISTS embedding_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    note_id TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    enqueued_at TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    UNIQUE(note_id),
    FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_embedding_queue_enqueued_at ON embedding_queue(enqueued_at);

INSERT OR IGNORE INTO local_metadata (key, value)
VALUES
    ('embedding_model_id', 'default-local'),
    ('embedding_dimensions', '384'),
    ('ai_index_schema_version', '1');
