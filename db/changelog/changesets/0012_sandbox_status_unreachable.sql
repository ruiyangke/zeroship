--liquibase formatted sql

-- accept `unreachable` status. Transcribed from
-- crates/sandbox/migrations/0002_sandbox_status_unreachable.sql
-- (sandbox.* → zeroship.*).
--
-- The Phase-1 restore path already UPDATEs sandboxes to
-- `status='unreachable'` when an agent fails to answer a signed
-- /version probe (see `restore::ProbeOutcome::Unreachable` →
-- `update_sandbox_status(SandboxStatus::Unreachable, …)`). The 0011
-- initial changeset declared the status CHECK without the value, so
-- every such UPDATE returned SQLSTATE 23514 and the row stayed at its
-- prior status. This changeset aligns the CHECK with the Rust enum
-- (`SandboxStatus::Unreachable`).
--
-- Pg auto-names anonymous CHECKs as `<table>_<column>_check` regardless
-- of schema, so the live constraint name is `sandboxes_status_check`.

--changeset zeroship-sandbox:sandbox-status-unreachable splitStatements:true
ALTER TABLE zeroship.sandboxes
    DROP CONSTRAINT IF EXISTS sandboxes_status_check;
ALTER TABLE zeroship.sandboxes
    ADD CONSTRAINT sandboxes_status_check
    CHECK (status IN ('starting', 'running', 'stopping', 'stopped',
                      'lost', 'recreating', 'orphan', 'unreachable'));
--rollback ALTER TABLE zeroship.sandboxes DROP CONSTRAINT IF EXISTS sandboxes_status_check;
