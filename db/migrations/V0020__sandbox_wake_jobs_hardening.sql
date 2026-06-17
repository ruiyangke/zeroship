-- security + perf hardening on wake_jobs (C-7-LT-PR2-FOLLOWUP).
-- Transcribed from crates/sandbox/migrations/0010_wake_jobs_hardening.sql
-- (sandbox.* → zeroship.*).
--
-- Three independent fixes flagged by round-17 reviewers:
--
--   R16-S1 (security): 0019 granted SELECT on wake_jobs to
--     `sandbox_audit`, which contradicts 0014's role-split invariant
--     (audit role is INSERT-only on events; no SELECT, no DELETE).
--     REVOKE that grant here.
--
--   R17-A2 (architecture, perf): adds a partial index on
--     `lessee_updated_at WHERE non-terminal` so the takeover sweep
--     (`WHERE state NOT IN ('ok','failed') AND lessee_updated_at < …`)
--     matches a supporting index instead of full-scanning.
--
--   R16-S3 (security): a column-level CHECK on `agent_url` forces the
--     URL shape (http(s) scheme; alphanumerics + a small punctuation set)
--     so a misbehaving controller cannot land arbitrary text in a column
--     `sandbox_app` SELECTs.
--
-- splitStatements:false: the REVOKE DO block + index + CHECK-add DO block
-- ship as one unit; the DO blocks contain `;` inside `$$`.
-- ─── R16-S1 ─────────────────────────────────────────────────────────
-- Revoke the audit-role SELECT 0019 granted by mistake.
-- The DO $$ block tolerates the role being absent in dev pg.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        REVOKE SELECT ON zeroship.wake_jobs FROM sandbox_audit;
    END IF;
END $$;

-- ─── R17-A2 ─────────────────────────────────────────────────────────
-- Partial index on `lessee_updated_at` filtered to non-terminal rows.
CREATE INDEX IF NOT EXISTS wake_jobs_lessee_idx
    ON zeroship.wake_jobs (lessee_updated_at)
    WHERE state NOT IN ('ok', 'failed');

-- ─── R16-S3 ─────────────────────────────────────────────────────────
-- Column-level CHECK on `agent_url`: must be NULL or a http(s)-scheme
-- URL composed of alphanumerics + the small punctuation set agent URLs
-- use (`. _ : / -`).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'zeroship'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_agent_url_chk'
    ) THEN
        ALTER TABLE zeroship.wake_jobs
            ADD CONSTRAINT wake_jobs_agent_url_chk
            CHECK (agent_url IS NULL OR agent_url ~ '^https?://[a-zA-Z0-9._:/-]+$');
    END IF;
END $$;
