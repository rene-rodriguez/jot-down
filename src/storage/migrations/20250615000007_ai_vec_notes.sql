-- sqlite-vec vector index. This migration is only applied after the vec0
-- module has been loaded and probed successfully.
CREATE VIRTUAL TABLE IF NOT EXISTS vec_notes USING vec0(
    note_id TEXT PRIMARY KEY,
    embedding FLOAT[384]
);
