-- Add content_hash column to local notes for change detection and conflict sync.
-- The remote (PostgreSQL) schema already carries content_hash; the local cache
-- was missing it, which broke get_note() and the sync push path.
ALTER TABLE notes ADD COLUMN content_hash TEXT NOT NULL DEFAULT '';
