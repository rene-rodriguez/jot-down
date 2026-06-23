-- Track when a failed sync_queue entry is next eligible for retry, so failures
-- back off exponentially instead of being retried every poll. NULL means "due
-- now" (never failed, or backoff cleared by a manual sync).
ALTER TABLE sync_queue ADD COLUMN next_attempt_at TEXT;
