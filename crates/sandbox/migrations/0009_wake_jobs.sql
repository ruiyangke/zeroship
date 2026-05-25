-- 0009_wake_jobs.sql — async wake-response state machine (C-7-LT-PR1)
--
-- Source-of-truth: docs/proposals/c7-lt-async-wake.md.
--
-- Backs the 202 Accepted + polling response shape. PR1 (this migration)
-- only LANDS the table; the controller does not write rows yet. PR2
-- wires the wake handler + state machine to insert and update.
--
-- Schema design:
--   - `wake_id`: typed-id-style key handed to the client; the client
--     polls /wake/<wake_id> to advance.
--   - `sandbox_id`: the sandbox being woken. Indexed for the
--     idempotency lookup ("does this sandbox already have a non-terminal
--     wake?").
--   - `state`: TEXT discriminator matching the WakeJobState enum
--     (sandbox/db.rs). Values: pending, reserving_slot, restoring,
--     livez_polling, clock_resyncing, registering, ok, failed.
--   - `error_code`: structured enum (NULL on success/in-flight) per
--     C-7-LT design Q3. Values: slot_unavailable,
--     source_teardown_timeout, restore_failed, livez_timeout,
--     clock_resync_failed, register_failed, internal.
--   - `error_message`: opaque human-readable (NULL on success/in-flight).
--   - timestamps: started_at, updated_at, ready_at (NULL until state=ok).
--   - `agent_url`: populated on state=ok so the client knows where to
--     route subsequent requests.
--   - `lessee` + `lessee_updated_at`: takeover discipline mirroring the
--     `sandboxes` table so a controller crash mid-wake doesn't strand
--     the row (PR2 will wire the takeover sweep).
--
-- Indexes:
--   - `wake_jobs_sandbox_idx` on `sandbox_id`: idempotency check
--     (find_pending_wake_for_sandbox).
--   - `wake_jobs_state_idx` on `state` WHERE non-terminal: keeps the
--     sweep index small (only mid-flight rows live in it).
--   - `wake_jobs_updated_at_idx` on `updated_at`: GC sweep
--     (gc_expired_wake_jobs) needs efficient range scan.
--
-- Forward-only. Idempotent via IF NOT EXISTS on every CREATE.

CREATE TABLE IF NOT EXISTS sandbox.wake_jobs (
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
-- typo in a controller write (or a future-migration code that forgets
-- to add a new variant here) errors at INSERT time instead of producing
-- a row the reader can't interpret.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.constraint_column_usage
        WHERE table_schema = 'sandbox'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_state_check'
    ) THEN
        ALTER TABLE sandbox.wake_jobs
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
        WHERE table_schema = 'sandbox'
          AND table_name = 'wake_jobs'
          AND constraint_name = 'wake_jobs_error_code_check'
    ) THEN
        ALTER TABLE sandbox.wake_jobs
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

-- Idempotency lookup: "does this sandbox already have a pending wake?"
CREATE INDEX IF NOT EXISTS wake_jobs_sandbox_idx
    ON sandbox.wake_jobs (sandbox_id);

-- Non-terminal sweep: takeover scan walks rows mid-flight only.
-- Partial index keeps it cheap regardless of fleet size.
CREATE INDEX IF NOT EXISTS wake_jobs_state_idx
    ON sandbox.wake_jobs (state)
    WHERE state NOT IN ('ok', 'failed');

-- GC sweep: gc_expired_wake_jobs needs efficient range scan on
-- updated_at to delete old terminal rows.
CREATE INDEX IF NOT EXISTS wake_jobs_updated_at_idx
    ON sandbox.wake_jobs (updated_at);

-- Grants. The `sandbox_app` role does all reads/writes on wake_jobs.
-- Audit role is read-only (consistent with the rest of the schema).
-- The DO $$ ... $$ block tolerates roles being absent in dev pg.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_app') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON sandbox.wake_jobs TO sandbox_app;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        GRANT SELECT ON sandbox.wake_jobs TO sandbox_audit;
    END IF;
END $$;
