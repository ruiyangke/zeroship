--liquibase formatted sql

-- extend `wake_jobs.error_code` CHECK to admit `agent_version_mismatch`
-- for the T5 restore-path agent /version fingerprint check. Transcribed
-- from
-- crates/sandbox/migrations/0014_wake_jobs_agent_version_mismatch_code.sql
-- (sandbox.* → zeroship.*).
--
-- 0019 / 0022 / 0023 built up the `wake_jobs.error_code` CHECK domain. T5
-- (signed `/version` fingerprint check after restore livez) introduces
-- `WakeErrorCode::AgentVersionMismatch` so the wake state machine can
-- write a structured code when the restored agent's `git_commit` does not
-- match the controller's own `BUILD_GIT_SHA`.
--
-- Why a distinct code: the failure is operator-actionable in a specific
-- way (partial fleet rollout in progress — wait, then retry) — distinct
-- from `livez_timeout` and `restore_failed`. Splitting it out lets the
-- SLO dashboard route rollout-skew failures away from the "real" buckets.
--
-- Implementation: drop `wake_jobs_error_code_check`, re-add with the
-- extended 10-value domain. Same shape as 0022 / 0023.
--changeset zeroship-sandbox:sandbox-wake-jobs-agent-version-mismatch-code splitStatements:false
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
                'staging_path_missing',
                'agent_version_mismatch'
            ));
    END IF;
END $$;
--rollback SELECT 1;
