-- 0007_sandbox_snapshot_columns.sql — sandboxes columns for snapshot
--
-- Source-of-truth: docs/proposals/sandbox-snapshot-restore.md § 9.1
-- (Migration B). Ships AFTER 0006 has soaked at least one release
-- (§ 13 step 2). All columns NULLable or DEFAULT, so Phase-3 code
-- paths that don't know about snapshots keep working.
--
-- Schema-pattern note: the proposal references column `state` and
-- `last_request_completed_at` but the actual schema uses `status` and
-- `last_used_at` (the latter from 0001:96, set on every successful
-- agent op). Proposal's intent maps:
--   proposal.state                       → schema.status
--   proposal.last_request_completed_at   → schema.last_used_at
--
-- Forward-only. Idempotent via `IF NOT EXISTS` on every ADD.

ALTER TABLE sandbox.sandboxes
    ADD COLUMN IF NOT EXISTS snapshot_artifact_path     TEXT,
    ADD COLUMN IF NOT EXISTS snapshot_taken_at          TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS snapshot_ch_version        TEXT,
    ADD COLUMN IF NOT EXISTS snapshot_sha256            BYTEA,
    ADD COLUMN IF NOT EXISTS snapshot_aead_dek_id       TEXT,
    ADD COLUMN IF NOT EXISTS snapshot_backing_versions  JSONB,
    ADD COLUMN IF NOT EXISTS snapshot_vm_index          SMALLINT,
    ADD COLUMN IF NOT EXISTS lessee_updated_at          TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_running_worker_id     TEXT,
    ADD COLUMN IF NOT EXISTS idle_snapshot_opted_in     BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS idle_snapshot_count_long_poll BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS last_drain_failure_at      TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS drain_failure_count        INTEGER NOT NULL DEFAULT 0;

-- Integrity guards for the snapshot artifact group.
-- A row in `snapshotted` MUST have an artifact path + sha + version;
-- a row NOT in any snapshot-related state should have all NULL. The
-- transient states (snapshotting, restoring, restoring_cold) are
-- mid-flight, so they may be partially populated.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'sandbox'
          AND table_name = 'sandboxes'
          AND constraint_name = 'sandboxes_snapshot_artifact_consistency'
    ) THEN
        ALTER TABLE sandbox.sandboxes
            ADD CONSTRAINT sandboxes_snapshot_artifact_consistency
            CHECK (
                -- snapshotted must carry the full artifact descriptor.
                (status NOT IN ('snapshotted','snapshotted_suspect')
                 OR (snapshot_artifact_path IS NOT NULL
                     AND snapshot_sha256 IS NOT NULL
                     AND snapshot_ch_version IS NOT NULL))
            );
    END IF;
END $$;

-- Lease-takeover scan for transient states (§ 6.1). Filtered partial
-- index keeps it small (only mid-flight rows live here).
CREATE INDEX IF NOT EXISTS sandboxes_status_lessee_idx
    ON sandbox.sandboxes (status, lessee_updated_at)
    WHERE status IN ('snapshotting','restoring','restoring_cold');

-- Idle-eviction sweep (§ 7). Only running + opted-in rows participate;
-- partial index makes the sweep query cheap regardless of fleet size.
CREATE INDEX IF NOT EXISTS sandboxes_idle_snapshot_idx
    ON sandbox.sandboxes (last_used_at)
    WHERE status = 'running' AND idle_snapshot_opted_in;
