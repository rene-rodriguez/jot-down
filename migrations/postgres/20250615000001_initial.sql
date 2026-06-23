-- PostgreSQL initial schema for Jot remote sync backend.
-- Applied via sqlx migrations when sync is enabled.

-- Notes table
CREATE TABLE IF NOT EXISTS notes (
    id          UUID PRIMARY KEY,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at  TIMESTAMPTZ,
    version     INTEGER NOT NULL DEFAULT 1,
    content_hash TEXT NOT NULL DEFAULT '',
    last_modified_by_device_id UUID NOT NULL
);

CREATE INDEX idx_notes_updated_at ON notes(updated_at);
CREATE INDEX idx_notes_deleted_at ON notes(deleted_at);

-- Tags table
CREATE TABLE IF NOT EXISTS tags (
    id   UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

-- Note-tag junction table
CREATE TABLE IF NOT EXISTS note_tags (
    note_id UUID NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    tag_id  UUID NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (note_id, tag_id)
);

-- Device registration
CREATE TABLE IF NOT EXISTS devices (
    id         UUID PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Sync events (append-only event log)
CREATE TABLE IF NOT EXISTS sync_events (
    id          BIGSERIAL PRIMARY KEY,
    entity_type TEXT NOT NULL,
    entity_id   UUID NOT NULL,
    operation   TEXT NOT NULL,       -- 'create', 'update', 'delete', 'tag_add', 'tag_remove'
    payload_json JSONB NOT NULL,
    device_id   UUID NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_sync_events_entity ON sync_events(entity_type, entity_id);
CREATE INDEX idx_sync_events_created_at ON sync_events(created_at);

-- Sync cursors (tracks which events each device has seen)
CREATE TABLE IF NOT EXISTS sync_cursors (
    device_id      UUID PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    last_event_id  BIGINT NOT NULL DEFAULT 0,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
