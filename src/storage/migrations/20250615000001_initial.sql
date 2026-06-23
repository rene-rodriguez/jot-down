CREATE TABLE IF NOT EXISTS notes (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    deleted_at TEXT
);

CREATE TABLE IF NOT EXISTS tags (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS note_tags (
    note_id TEXT NOT NULL,
    tag_id TEXT NOT NULL,
    PRIMARY KEY (note_id, tag_id),
    FOREIGN KEY (note_id) REFERENCES notes(id),
    FOREIGN KEY (tag_id) REFERENCES tags(id)
);

CREATE INDEX IF NOT EXISTS idx_notes_deleted_at ON notes(deleted_at);
CREATE INDEX IF NOT EXISTS idx_notes_updated_at ON notes(updated_at);
CREATE INDEX IF NOT EXISTS idx_note_tags_note_id ON note_tags(note_id);
CREATE INDEX IF NOT EXISTS idx_note_tags_tag_id ON note_tags(tag_id);
