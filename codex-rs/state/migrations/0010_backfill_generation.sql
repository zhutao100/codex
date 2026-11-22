ALTER TABLE backfill_state ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;

-- A previous writer could fall back to JSONL without invalidating a complete index.
UPDATE backfill_state SET status = 'pending', last_watermark = NULL, last_success_at = NULL WHERE id = 1;
