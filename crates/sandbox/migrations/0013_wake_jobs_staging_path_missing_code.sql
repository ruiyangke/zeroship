-- 0013_wake_jobs_staging_path_missing_code.sql — extend
-- `wake_jobs.error_code` CHECK to admit `staging_path_missing` for
-- the R23-API1 / R25-S1 / R25-I1 / R25-I2 controller-side disk-image
-- staging preflight rejection variant.
--
-- Background. Migrations 0009 and 0012 created (and then extended)
-- the `wake_jobs.error_code` CHECK domain. The eight values landed by
-- 0012 cover every failure mode the wake state machine had at the
-- time. The R23-API1 + R25-S1 + R25-I1 + R25-I2 convergent fix adds
-- `WakeErrorCode::StagingPathMissing` so the `submit_restore_job`
-- preflight (workspace.img + user_home.img `assert_disk_image_present`)
-- can write a structured error code instead of folding into the
-- generic `restore_failed` bucket.
--
-- Why a distinct code:
--   - The failure is operator-actionable (re-stage from snapshot
--     store / restore from backup) — distinct from the `restore_failed`
--     bucket which today routes to "alloc-level Nomad failure, retry".
--   - SECURITY (R25-S1): paths must not appear in the free-text
--     `error_message` column because an `AdminRole::ReadOnly` bearer
--     can read it via `GET /admin/sandboxes/{id}/wake/{wake_id}`. The
--     structured code carries the routing decision; `error_message`
--     carries only the user-safe summary "staging image missing:
--     <which> for <typed_sandbox_id>".
--
-- Internal pg-column form is `staging_path_missing` (the failure
-- shape: "a path was missing"). The wire code emitted by
-- `WakeErrorCode::wire_code` is `staging_image_missing` (the resource
-- name) — they differ intentionally; the wire side reads in operator
-- language, the column side reads in code language.
--
-- Implementation:
--   1. Drop `wake_jobs_error_code_check` (enumerable).
--   2. Re-add it with the extended 9-value domain.
--
-- Forward-only. Idempotent. Same shape as 0012.

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
                'wake_worker_aborted',
                'staging_path_missing'
            ));
    END IF;
END $$;
