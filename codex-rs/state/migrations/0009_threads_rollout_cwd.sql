ALTER TABLE threads ADD COLUMN rollout_cwd TEXT;

-- Older backfills checkpointed past failed files. Rebuild the derived index
-- from rollouts without dropping the database, its memories, or its logs.
UPDATE backfill_state SET status = 'pending', last_watermark = NULL, last_success_at = NULL WHERE id = 1;
