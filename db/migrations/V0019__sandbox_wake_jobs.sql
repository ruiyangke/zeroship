-- async wake-response state machine (C-7-LT-PR1). Transcribed from
-- crates/sandbox/migrations/0009_wake_jobs.sql (sandbox.* → zeroship.*).
--
-- Source-of-truth: docs/proposals/c7-lt-async-wake.md. Backs the 202
-- Accepted + polling response shape.
--
-- Schema design:
--   - `wake_id`: typed-id-style key handed to the client; the client
--     polls /wake/<wake_id> to advance.
--   - `sandbox_id`: the sandbox being woken. Indexed for the idempotency
--     lookup.
--   - `state`: TEXT discriminator matching the WakeJobState enum.
--   - `error_code`: structured enum (NULL on success/in-flight).
--   - `error_message`: opaque human-readable (NULL on success/in-flight).
--   - timestamps: started_at, updated_at, ready_at (NULL until state=ok).
--   - `agent_url`: populated on state=ok.
--   - `lessee` + `lessee_updated_at`: takeover discipline mirroring the
--     `sandboxes` table.
--
-- splitStatements:false: the CREATE TABLE + DO-block CHECK guards +
-- indexes + the grant DO block ship as one unit; the DO blocks contain
-- `;` inside `$$`.
CREATE TABLE IF NOT EXISTS zeroship.wake_jobs (
    wake_id           TEXT         PRIMARY KEY,
    sandbox_id        TEXT         NOT NULL,
    state             TEXT         NOT NULL,
    error_code        TEXT,
    error_message     TEXT,
    started_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    ready_at          TIMESTAMPTZ,
    agent_url         TEXT,
    lessee            TEXT         NOT NULL,
    lessee_updated_at TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- State CHECK — keep enum domain validated at the database level. Any
-- typo in a controller write errors at INSERT time instead of producing
-- a row the reader can't interpret.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'zeroship'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_state_check'
    ) THEN
        ALTER TABLE zeroship.wake_jobs
            ADD CONSTRAINT wake_jobs_state_check
            CHECK (state IN (
                'pending',
                'reserving_slot',
                'restoring',
                'livez_polling',
                'clock_resyncing',
                'registering',
                'ok',
                'failed'
            ));
    END IF;
END $$;

-- error_code CHECK — same rationale as state. NULL is allowed (set only
-- when state='failed').
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'zeroship'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_error_code_check'
    ) THEN
        ALTER TABLE zeroship.wake_jobs
            ADD CONSTRAINT wake_jobs_error_code_check
            CHECK (error_code IS NULL OR error_code IN (
                'slot_unavailable',
                'source_teardown_timeout',
                'restore_failed',
                'livez_timeout',
                'clock_resync_failed',
                'register_failed',
                'internal'
            ));
    END IF;
END $$;

-- sandbox_id CHECK — enforce the typed-id shape (matches zeroship.sandboxes
-- and zeroship.sandbox_events). Rejects a malformed sandbox_id at INSERT time.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'zeroship'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_sandbox_id_check'
    ) THEN
        ALTER TABLE zeroship.wake_jobs
            ADD CONSTRAINT wake_jobs_sandbox_id_check
            CHECK (sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$');
    END IF;
END $$;

-- Idempotency lookup: "does this sandbox already have a pending wake?"
CREATE INDEX IF NOT EXISTS wake_jobs_sandbox_idx
    ON zeroship.wake_jobs (sandbox_id);

-- Non-terminal sweep: takeover scan walks rows mid-flight only.
-- Partial index keeps it cheap regardless of fleet size.
CREATE INDEX IF NOT EXISTS wake_jobs_state_idx
    ON zeroship.wake_jobs (state)
    WHERE state NOT IN ('ok', 'failed');

-- GC sweep: gc_expired_wake_jobs needs efficient range scan on
-- updated_at to delete old terminal rows.
CREATE INDEX IF NOT EXISTS wake_jobs_updated_at_idx
    ON zeroship.wake_jobs (updated_at);

-- Grants. The `sandbox_app` role does all reads/writes on wake_jobs.
-- Audit role is read-only (consistent with the rest of the schema). The
-- DO $$ ... $$ block tolerates roles being absent in dev pg.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_app') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.wake_jobs TO sandbox_app;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        GRANT SELECT ON zeroship.wake_jobs TO sandbox_audit;
    END IF;
END $$;
