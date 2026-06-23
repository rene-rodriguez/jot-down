-- Track when a failed embedding_queue job is next eligible for retry, so embed
-- failures back off exponentially instead of being retried on every batch. NULL
-- means "due now" (never failed, or cleared by a fresh edit / reindex).
ALTER TABLE embedding_queue ADD COLUMN next_attempt_at TEXT;
