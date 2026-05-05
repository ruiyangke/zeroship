-- 0005_events_sandbox_id_nullable.sql — Round-4 / IMPORTANT #4
--
-- Pre-fix, `sandbox.events.sandbox_id` was `NOT NULL` with a CHECK
-- requiring the typed-id shape `^sbx_[...]$`. That forced the GDPR
-- delete audit row (kind = 'gdpr.delete_user') to carry SOMETHING in
-- `sandbox_id`. When the deleted user had at least one sandbox the
-- code reused one of the deleted ids; when the user had zero
-- sandboxes it minted a brand-new typed-id with `generate("sbx")` and
-- inserted it as the audit row's `sandbox_id`. That synthetic id
-- never matched a real sandbox row but DID land in
-- `idx_events_sandbox_ts`, polluting the per-sandbox lookup index
-- with unmatchable keys for the lifetime of the partition.
--
-- This migration relaxes the column to `NULL`-able. The audit-row
-- writer in `delete_user` then inserts `sandbox_id = NULL` when the
-- event isn't tied to a specific sandbox. The existing CHECK
-- constraint accepts NULL by default (CHECK fires only on non-NULL
-- values), so no constraint change is needed beyond dropping
-- `NOT NULL`. The index (`idx_events_sandbox_ts`) silently treats
-- NULL as a non-indexed value via the BTREE NULL semantics — pg
-- still indexes them but they don't collide with real sandbox ids.
--
-- Forward-only + idempotent: each ALTER … DROP NOT NULL is wrapped
-- in a DO block so a replay finds the column already-nullable and
-- no-ops.

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM information_schema.columns
         WHERE table_schema = 'sandbox'
           AND table_name   = 'events'
           AND column_name  = 'sandbox_id'
           AND is_nullable  = 'NO'
    ) THEN
        EXECUTE 'ALTER TABLE sandbox.events ALTER COLUMN sandbox_id DROP NOT NULL';
    END IF;
END
$$;
