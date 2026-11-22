-- Old index rows materialized the scanning process's default for headers
-- without a provider. Recover provenance from JSONL without touching logs or memories.
ALTER TABLE threads ADD COLUMN model_provider_inferred INTEGER NOT NULL DEFAULT 0;

UPDATE backfill_state
SET status = 'pending', last_watermark = NULL, last_success_at = NULL
WHERE id = 1;
