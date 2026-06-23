-- Add remote_version column for optimistic concurrency
ALTER TABLE notes ADD COLUMN remote_version INTEGER NOT NULL DEFAULT 0;

-- Track unresolved conflicts between local and remote
CREATE TABLE IF NOT EXISTS local_conflicts (
    id TEXT PRIMARY KEY NOT NULL,
    note_id TEXT NOT NULL,
    local_payload_json TEXT NOT NULL,
    remote_payload_json TEXT NOT NULL,
    base_version INTEGER NOT NULL DEFAULT 0,
    detected_at TEXT NOT NULL,
    resolved_at TEXT,
    resolution TEXT,
    FOREIGN KEY (note_id) REFERENCES notes(id)
);

CREATE INDEX IF NOT EXISTS idx_conflicts_note_id ON local_conflicts(note_id);
CREATE INDEX IF NOT EXISTS idx_conflicts_unresolved ON local_conflicts(resolved_at);
