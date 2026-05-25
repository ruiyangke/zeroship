-- 0011_wake_jobs_unique.sql — GATE-C2 (R17-C2): TOCTOU close-off on the
-- wake-POST handler.
--
-- Concurrency r17 finding R17-C2 (CRITICAL): between
-- `find_pending_wake_for_sandbox` (`admin_handlers.rs:1556`) and
-- `insert_wake_job` (`:1642`) there is no transaction and no unique
-- constraint on the sandbox dimension. Two concurrent POSTs both miss
-- the precheck, both insert, and both spawn a `WakeMachine` — the
-- loser's `rollback_with` then calls `teardown_restore`, releasing the
-- WINNER's vm_index (identical fingerprint to the R10-C1 race shape on
-- `sandboxes`).
--
-- Low-probability at c=1 smoke but high at c=20+ stress.
--
-- Fix: a partial UNIQUE INDEX over `sandbox_id` filtered to
-- non-terminal states. Postgres treats `WHERE` predicates on partial
-- indexes as part of the uniqueness scope, so two non-terminal rows
-- for the same sandbox cannot coexist. Once both rows are terminal
-- ('ok' / 'failed') they drop out of the index and a fresh wake can
-- be POSTed for that sandbox.
--
-- The `db.rs::insert_wake_job` path uses
-- `ON CONFLICT (sandbox_id) WHERE state NOT IN ('ok','failed')
-- DO NOTHING` so a race-loser detects the collision atomically
-- without bubbling a 500 to the client; the handler then re-reads
-- the winner's row and returns `replay: true`.
--
-- Forward-only. Idempotent via `IF NOT EXISTS`.

CREATE UNIQUE INDEX IF NOT EXISTS wake_jobs_sandbox_pending_uniq
    ON sandbox.wake_jobs (sandbox_id)
    WHERE state NOT IN ('ok', 'failed');
