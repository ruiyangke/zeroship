-- 0010_wake_jobs_hardening.sql — security + perf hardening on wake_jobs
-- (C-7-LT-PR2-FOLLOWUP).
--
-- Three independent fixes flagged by round-17 reviewers:
--
--   R16-S1 (security): 0009 granted SELECT on wake_jobs to
--     `sandbox_audit`, which contradicts 0004's role-split invariant
--     (audit role is INSERT-only on events; no SELECT, no DELETE).
--     REVOKE that grant here. If a future read-only audit-reader
--     role lands, it gets its own role per 0004's design.
--
--   R17-A2 (architecture, perf): 0009's `wake_jobs_state_idx` was
--     named for the takeover scan but only indexes `state`. PR2's
--     takeover sweep filters `WHERE state NOT IN ('ok','failed')
--     AND lessee_updated_at < now() - threshold`; without an index
--     on `lessee_updated_at`, pg full-scans every non-terminal row
--     and filters in-memory. Adds a partial index on
--     `lessee_updated_at WHERE non-terminal` so the sweep matches
--     the lessee-CAS pattern used by `sandboxes.lessee_updated_at`.
--
--   R16-S3 (security): `agent_url` is free-form TEXT; PR2 wires it
--     from `backend.derive_agent_url()`, which today returns
--     `http://10.x.y.z:7000` but is a controller-side string. A
--     column-level CHECK constraint forces the URL shape (http(s)
--     scheme; alphanumerics + a small punctuation set) so a
--     misbehaving controller cannot land arbitrary text in a column
--     `sandbox_app` SELECTs.
--
-- Forward-only. Idempotent (REVOKE is safe on absent grants,
-- IF NOT EXISTS guards the index, the constraint-add is wrapped in
-- an existence check).

-- ─── R16-S1 ─────────────────────────────────────────────────────────
-- Revoke the audit-role SELECT 0009 granted by mistake.
-- The DO $$ block tolerates the role being absent in dev pg.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        REVOKE SELECT ON sandbox.wake_jobs FROM sandbox_audit;
    END IF;
END $$;

-- ─── R17-A2 ─────────────────────────────────────────────────────────
-- Partial index on `lessee_updated_at` filtered to non-terminal
-- rows. PR2's takeover sweep against `wake_jobs` now matches a
-- supporting index instead of full-scanning the non-terminal
-- partition.
CREATE INDEX IF NOT EXISTS wake_jobs_lessee_idx
    ON sandbox.wake_jobs (lessee_updated_at)
    WHERE state NOT IN ('ok', 'failed');

-- ─── R16-S3 ─────────────────────────────────────────────────────────
-- Column-level CHECK on `agent_url`: must be NULL or a
-- http(s)-scheme URL composed of alphanumerics + the small
-- punctuation set agent URLs use (`. _ : / -`).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'sandbox'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_agent_url_chk'
    ) THEN
        ALTER TABLE sandbox.wake_jobs
            ADD CONSTRAINT wake_jobs_agent_url_chk
            CHECK (agent_url IS NULL OR agent_url ~ '^https?://[a-zA-Z0-9._:/-]+$');
    END IF;
END $$;
