-- extend `wake_jobs.error_code` CHECK to admit `wake_worker_aborted` for
-- the R19-C1 takeover sweep. Transcribed from
-- crates/sandbox/migrations/0012_wake_jobs_aborted_code.sql
-- (sandbox.* → zeroship.*).
--
-- 0019 created `wake_jobs.error_code` with a strict CHECK domain matching
-- the seven WakeErrorCode variants. The R19-C1 takeover sweep flips
-- abandoned rows to `state = 'failed'` so the partial UNIQUE INDEX (0021)
-- releases; a dedicated `wake_worker_aborted` error_code distinguishes the
-- takeover failure from the in-flight wake's own failure modes so clients
-- can retry IMMEDIATELY.
--
-- Implementation: drop `wake_jobs_error_code_check`, re-add with the
-- extended domain. splitStatements:false because the DO blocks contain
-- `;` inside `$$`.
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
                'wake_worker_aborted'
            ));
    END IF;
END $$;
