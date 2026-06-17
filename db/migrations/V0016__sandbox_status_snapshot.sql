-- accept snapshot lifecycle states. Transcribed from
-- crates/sandbox/migrations/0006_sandbox_status_snapshot.sql
-- (sandbox.* → zeroship.*).
--
-- Source-of-truth: docs/proposals/sandbox-snapshot-restore.md § 9.2
-- (state machine) + § 13 rollout step 1 (Migration A — enum-only).
--
-- The schema uses TEXT + CHECK rather than a Postgres native ENUM type,
-- so this mirrors 0012's approach: DROP the existing constraint, ADD a
-- new one with the expanded value list.
--
-- New values added (6): snapshotting, snapshotted, snapshotting_aborted,
-- snapshotted_suspect, restoring, restoring_cold.
--
-- Pg auto-names the inline CHECK `sandboxes_status_check` regardless of
-- schema.

ALTER TABLE zeroship.sandboxes
    DROP CONSTRAINT IF EXISTS sandboxes_status_check;
ALTER TABLE zeroship.sandboxes
    ADD CONSTRAINT sandboxes_status_check
    CHECK (status IN ('starting', 'running', 'stopping', 'stopped',
                      'lost', 'recreating', 'orphan', 'unreachable',
                      'snapshotting', 'snapshotted', 'snapshotting_aborted',
                      'snapshotted_suspect', 'restoring', 'restoring_cold'));
