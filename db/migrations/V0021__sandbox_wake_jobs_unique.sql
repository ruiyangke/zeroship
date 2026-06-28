-- GATE-C2 (R17-C2): TOCTOU close-off on the wake-POST handler.
-- Transcribed from crates/sandbox/migrations/0011_wake_jobs_unique.sql
-- (sandbox.* → zeroship.*).
--
-- Between `find_pending_wake_for_sandbox` and `insert_wake_job` there is
-- no transaction and no unique constraint on the sandbox dimension. Two
-- concurrent POSTs both miss the precheck, both insert, and both spawn a
-- `WakeMachine` — the loser's `rollback_with` then releases the WINNER's
-- vm_index.
--
-- Fix: a partial UNIQUE INDEX over `sandbox_id` filtered to non-terminal
-- states. Postgres treats `WHERE` predicates on partial indexes as part
-- of the uniqueness scope, so two non-terminal rows for the same sandbox
-- cannot coexist. Once both rows are terminal ('ok' / 'failed') they drop
-- out of the index and a fresh wake can be POSTed.

CREATE UNIQUE INDEX IF NOT EXISTS wake_jobs_sandbox_pending_uniq
    ON zeroship.wake_jobs (sandbox_id)
    WHERE state NOT IN ('ok', 'failed');
