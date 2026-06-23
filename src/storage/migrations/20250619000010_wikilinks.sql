-- Explicit `[[wikilink]]` index, so backlinks ("notes linking *to* this one")
-- are an O(1) lookup instead of a full-body scan at preview time. One row per
-- (source note, normalized target title). `dst_id` is resolved when a note with
-- that title exists, else NULL for a dangling link (a first-class state — the
-- target may be created later, at which point the row is back-filled).
CREATE TABLE IF NOT EXISTS note_links (
    src_id    TEXT NOT NULL,
    dst_title TEXT NOT NULL,
    dst_id    TEXT,
    PRIMARY KEY (src_id, dst_title)
);

-- Backlink queries pivot on the resolved target id.
CREATE INDEX IF NOT EXISTS idx_note_links_dst_id ON note_links(dst_id);
-- Re-resolution on note create/rename pivots on the normalized title.
CREATE INDEX IF NOT EXISTS idx_note_links_dst_title ON note_links(dst_title);
