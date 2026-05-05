-- 0002_sandbox_status_unreachable.sql — accept `unreachable` status
--
-- The Phase-1 restore path already UPDATEs sandboxes to
-- `status='unreachable'` when an agent fails to answer a signed
-- /version probe (see `restore::ProbeOutcome::Unreachable` →
-- `update_sandbox_status(SandboxStatus::Unreachable, …)`). Migration
-- 0001 declared the status CHECK without the value, so every such
-- UPDATE returned SQLSTATE 23514 and the row stayed at its prior
-- status. This migration aligns the CHECK with the Rust enum
-- (`SandboxStatus::Unreachable`).
--
-- Forward-only and idempotent: DROP CONSTRAINT IF EXISTS first so a
-- re-run on a partially-applied schema is a no-op.

-- Pg auto-names anonymous CHECKs as `<table>_<column>_check`. The
-- 0001 migration declared an inline `CHECK (status IN (...))` so the
-- live constraint name is `sandboxes_status_check`. (Round-2 fixer /
-- MINOR #5: pre-fix this also dropped `sandbox_sandboxes_status_check`
-- which is unreachable — pg never auto-names with the schema prefix.
-- The IF EXISTS made the redundant DROP a no-op, but the false
-- second name was misleading; removed.)
ALTER TABLE sandbox.sandboxes
    DROP CONSTRAINT IF EXISTS sandboxes_status_check;

ALTER TABLE sandbox.sandboxes
    ADD CONSTRAINT sandboxes_status_check
    CHECK (status IN ('starting', 'running', 'stopping', 'stopped',
                      'lost', 'recreating', 'orphan', 'unreachable'));
