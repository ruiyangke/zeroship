-- 0014_wake_jobs_agent_version_mismatch_code.sql — extend
-- `wake_jobs.error_code` CHECK to admit `agent_version_mismatch` for
-- the T5 restore-path agent /version fingerprint check.
--
-- Background. Migrations 0009 / 0012 / 0013 built up the
-- `wake_jobs.error_code` CHECK domain. T5 (signed `/version`
-- fingerprint check after restore livez) introduces a new variant
-- `WakeErrorCode::AgentVersionMismatch` so the wake state machine can
-- write a structured code when the restored agent's `git_commit` does
-- not match the controller's own `BUILD_GIT_SHA`. Without this
-- migration the new variant cannot land in the column (the existing
-- CHECK domain rejects it).
--
-- Why a distinct code:
--   - Failure is operator-actionable in a specific way (partial fleet
--     rollout in progress — wait for the rollout to finish, then
--     retry the wake) — distinct from `livez_timeout` ("agent never
--     came up", probably alloc-level) and `restore_backend_failed`
--     ("ch-remote restore itself failed").
--   - Splitting it out lets the SLO dashboard route rollout-skew
--     failures away from the "real" wake-failure buckets so a
--     deploy-in-progress doesn't spike the agent-unhealthy SLO.
--
-- Implementation:
--   1. Drop `wake_jobs_error_code_check` (enumerable).
--   2. Re-add it with the extended 10-value domain.
--
-- Forward-only. Idempotent. Same shape as 0012 / 0013.

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
                'staging_path_missing',
                'agent_version_mismatch'
            ));
    END IF;
END $$;
