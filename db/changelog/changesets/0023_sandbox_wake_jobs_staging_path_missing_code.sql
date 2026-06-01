--liquibase formatted sql

-- extend `wake_jobs.error_code` CHECK to admit `staging_path_missing` for
-- the R23-API1 / R25-S1 / R25-I1 / R25-I2 controller-side disk-image
-- staging preflight rejection variant. Transcribed from
-- crates/sandbox/migrations/0013_wake_jobs_staging_path_missing_code.sql
-- (sandbox.* → zeroship.*).
--
-- 0019 and 0022 created (and extended) the `wake_jobs.error_code` CHECK
-- domain. This adds `WakeErrorCode::StagingPathMissing` so the
-- `submit_restore_job` preflight (workspace.img + user_home.img
-- `assert_disk_image_present`) can write a structured error code instead
-- of folding into the generic `restore_failed` bucket.
--
-- Why a distinct code: the failure is operator-actionable (re-stage from
-- snapshot store / restore from backup); and SECURITY (R25-S1) — paths
-- must not appear in the free-text `error_message` column (an
-- AdminRole::ReadOnly bearer can read it). Internal pg-column form is
-- `staging_path_missing`; the wire code (`WakeErrorCode::wire_code`) is
-- `staging_image_missing` — they differ intentionally.
--
-- Implementation: drop `wake_jobs_error_code_check`, re-add with the
-- extended 9-value domain. Same shape as 0022.
--changeset zeroship-sandbox:sandbox-wake-jobs-staging-path-missing-code splitStatements:false
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'zeroship'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_error_code_check'
    ) THEN
        ALTER TABLE zeroship.wake_jobs
            DROP CONSTRAINT wake_jobs_error_code_check;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'zeroship'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_error_code_check'
    ) THEN
        ALTER TABLE zeroship.wake_jobs
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
--rollback SELECT 1;
