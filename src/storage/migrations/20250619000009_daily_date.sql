-- Record the calendar date (ISO `YYYY-MM-DD`, local time) of notes created via
-- the daily-note path. This is the honest source of truth for "which day is
-- this note for" — far more robust than reverse-parsing titles through a
-- user-configurable `daily_format`. NULL for every non-daily note. Used to find
-- the most recent prior daily note (task rollups) and "on this day" matches.
ALTER TABLE notes ADD COLUMN daily_date TEXT;
