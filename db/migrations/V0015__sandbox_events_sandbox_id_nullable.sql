-- relax zeroship.sandbox_events.sandbox_id to NULLable. Transcribed from
-- crates/sandbox/migrations/0005_events_sandbox_id_nullable.sql
-- (sandbox.* → zeroship.*).
--
-- Pre-fix, `sandbox_events.sandbox_id` was `NOT NULL` with a CHECK requiring the
-- typed-id shape `^sbx_[...]$`. That forced the GDPR delete audit row
-- (kind = 'gdpr.delete_user') to carry SOMETHING in `sandbox_id`; when
-- the deleted user had zero sandboxes it minted a synthetic typed-id that
-- never matched a real sandbox row but DID land in `idx_sandbox_events_sandbox_ts`,
-- polluting the per-sandbox lookup index.
--
-- This relaxes the column to NULLable. The audit-row writer in
-- `delete_user` then inserts `sandbox_id = NULL` when the event isn't tied
-- to a specific sandbox. The existing CHECK accepts NULL by default (CHECK
-- fires only on non-NULL values), so no constraint change is needed beyond
-- dropping NOT NULL.
--
-- Wrapped in a DO block so a replay finds the column already-nullable and
-- no-ops. splitStatements:false because the DO block contains `;` inside
-- `$$`.

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM information_schema.columns
         WHERE table_schema = 'zeroship'
           AND table_name   = 'sandbox_events'
           AND column_name  = 'sandbox_id'
           AND is_nullable  = 'NO'
    ) THEN
        EXECUTE 'ALTER TABLE zeroship.sandbox_events ALTER COLUMN sandbox_id DROP NOT NULL';
    END IF;
END
$$;
