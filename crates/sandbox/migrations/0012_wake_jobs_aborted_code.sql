-- 0012_wake_jobs_aborted_code.sql — extend `wake_jobs.error_code` CHECK
-- to admit `wake_worker_aborted` for the R19-C1 takeover sweep.
--
-- Background. Migration 0009 created `wake_jobs.error_code` with a
-- strict CHECK domain matching the seven `WakeErrorCode` variants
-- defined in `crates/sandbox/src/db.rs` at the time of PR1. R17-A1
-- shipped the per-transition `lessee_updated_at` bump, but until R19
-- nothing READ that timestamp — so a controller crash mid-wake left
-- the row in a non-terminal state forever. The GATE-C2 UNIQUE INDEX
-- (migration 0011) then blocks every subsequent wake POST for that
-- sandbox: permanent wedge.
--
-- R19-C1 fix wires a takeover sweep that flips abandoned rows to
-- `state = 'failed'` so the partial UNIQUE INDEX releases. A
-- dedicated `wake_worker_aborted` error_code distinguishes the
-- takeover failure from the in-flight wake's own failure modes:
-- clients today branch on `internal_error` as "back off and retry";
-- splitting `wake_worker_aborted` out lets clients retry IMMEDIATELY
-- (the sandbox is now free) and lets the SLO dashboard tell the two
-- apart.
--
-- Implementation:
--   1. Drop `wake_jobs_error_code_check` (it's enumerable).
--   2. Re-add it with the extended domain.
--
-- Forward-only. Idempotent: `IF EXISTS` on the drop, `IF NOT EXISTS`-
-- shaped re-add wrapped in `information_schema` existence check —
-- same pattern 0009 uses for its constraint adds.

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'sandbox'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_error_code_check'
    ) THEN
        ALTER TABLE sandbox.wake_jobs
            DROP CONSTRAINT wake_jobs_error_code_check;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'sandbox'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_error_code_check'
    ) THEN
        ALTER TABLE sandbox.wake_jobs
            ADD CONSTRAINT wake_jobs_error_code_check
            CHECK (error_code IS NULL OR error_code IN (
                'slot_unavailable',
                'source_teardown_timeout',
                'restore_failed',
                'livez_timeout',
                'clock_resync_failed',
                'register_failed',
                'internal',
                'wake_worker_aborted'
            ));
    END IF;
END $$;
