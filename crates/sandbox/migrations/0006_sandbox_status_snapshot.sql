-- 0006_sandbox_status_snapshot.sql — accept snapshot lifecycle states
--
-- Source-of-truth: docs/proposals/sandbox-snapshot-restore.md § 9.2
-- (state machine) + § 13 rollout step 1 (Migration A only — enum-only,
-- no code changes that WRITE these states yet).
--
-- The schema uses TEXT + CHECK rather than a Postgres native ENUM type
-- (see 0001/0002 — Pg `ALTER TYPE ADD VALUE` was rejected during the
-- Phase-1 design in favor of CHECK simplicity). So this migration
-- mirrors 0002's approach: DROP the existing constraint, ADD a new
-- one with the expanded value list.
--
-- New values added (6):
--   snapshotting          — pause + snapshot in progress
--   snapshotted           — eviction complete; L2 artifact present
--   snapshotting_aborted  — controller crashed mid-snapshot; lease-takeover state
--   snapshotted_suspect   — restore failed unrecoverably; needs cold-boot fallback
--   restoring             — fetching artifact + spawning CH --restore
--   restoring_cold        — cold-boot fallback because snapshot was suspect
--
-- Rollout discipline (§ 13 step 1): this migration ships ALONE — no
-- handler code that writes these states ships in the same release.
-- Read-tolerant code (lease-takeover query that matches on these
-- values, allocator scan that ignores them) lands in the next PR
-- alongside Migration B (the column additions).
--
-- Forward-only and idempotent: `DROP CONSTRAINT IF EXISTS` makes a
-- partial replay a no-op. Re-applying after the new constraint is
-- already in place: the DROP succeeds (constraint exists), the ADD
-- re-adds the same constraint — net no-op.

ALTER TABLE sandbox.sandboxes
    DROP CONSTRAINT IF EXISTS sandboxes_status_check;

ALTER TABLE sandbox.sandboxes
    ADD CONSTRAINT sandboxes_status_check
    CHECK (status IN ('starting', 'running', 'stopping', 'stopped',
                      'lost', 'recreating', 'orphan', 'unreachable',
                      'snapshotting', 'snapshotted', 'snapshotting_aborted',
                      'snapshotted_suspect', 'restoring', 'restoring_cold'));
