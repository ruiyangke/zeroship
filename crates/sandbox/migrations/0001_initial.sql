-- 0001_initial.sql — pg-backed sandbox state (Phase 0)
--
-- Source-of-truth: docs/proposals/sandbox-pg-state.md § 6 (Schema).
-- Round-7 design v8.
--
-- This migration is forward-only and idempotent. It runs under the
-- `sandbox_admin` role (DDL) at boot via the designated-migrator
-- pattern (§ 7.1). The bookkeeping INSERT into
-- `sandbox.schema_migrations` is performed by the migration runner,
-- NOT by this file.
--
-- Idempotency: every CREATE uses IF NOT EXISTS so a partial replay
-- (e.g. two migrators racing — § 7.1's UNIQUE-constraint
-- race-tolerance fallback) is a no-op past the first apply.
--
-- Roles (sandbox_admin / sandbox_app / sandbox_audit / sandbox_gdpr,
-- § 13.2) are created in a DO $$ ... $$ block so the migration runs
-- on restricted CI Postgres (where the calling role lacks superuser).
-- In production, the operator deploys the four roles via a separate
-- bootstrap step; this DO block then no-ops because the roles already
-- exist.

-- ─── Schema ──────────────────────────────────────────────────────────

CREATE SCHEMA IF NOT EXISTS sandbox;

-- ─── 6.1 sandbox.schema_migrations ───────────────────────────────────

CREATE TABLE IF NOT EXISTS sandbox.schema_migrations (
    version     BIGINT       PRIMARY KEY,
    applied_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
    sha256      TEXT         NOT NULL,
    description TEXT         NOT NULL DEFAULT ''
);

-- ─── 6.2 sandbox.hosts ───────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS sandbox.hosts (
    host_id          TEXT         PRIMARY KEY
                                  CHECK (host_id ~ '^hst_[0-9A-Za-z]{20,40}$'),
    boot_id          TEXT         NOT NULL
                                  CHECK (boot_id ~ '^[0-9A-Za-z]{20,40}$'),
    hostname         TEXT         NOT NULL,
    region           TEXT         NOT NULL
                                  CHECK (region ~ '^[a-z]{2}-[a-z]+-[0-9]+$'),
    backend          TEXT         NOT NULL
                                  CHECK (backend IN ('docker', 'k8s', 'nomad-ch')),
    started_at       TIMESTAMPTZ  NOT NULL DEFAULT now(),
    last_heartbeat   TIMESTAMPTZ  NOT NULL DEFAULT now(),
    status           TEXT         NOT NULL DEFAULT 'alive'
                                  CHECK (status IN ('alive', 'draining', 'dead')),
    drain_started_at TIMESTAMPTZ  NULL,
    version          TEXT         NOT NULL DEFAULT '',
    metadata         JSONB        NOT NULL DEFAULT '{}'::JSONB
);

CREATE INDEX IF NOT EXISTS idx_hosts_status_heartbeat
    ON sandbox.hosts (status, last_heartbeat);
CREATE INDEX IF NOT EXISTS idx_hosts_region_status
    ON sandbox.hosts (region, status);

-- ─── 6.3 sandbox.sandboxes ───────────────────────────────────────────

CREATE TABLE IF NOT EXISTS sandbox.sandboxes (
    sandbox_id     TEXT         PRIMARY KEY
                                CHECK (sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'),
    user_id        TEXT         NOT NULL
                                CHECK (user_id ~ '^usr_[0-9A-Za-z]{20,40}$'),
    project_id     TEXT         NOT NULL
                                CHECK (project_id ~ '^prj_[0-9A-Za-z]{20,40}$'),
    backend        TEXT         NOT NULL
                                CHECK (backend IN ('docker', 'k8s', 'nomad-ch')),
    vm_index       INTEGER      NULL,
    agent_url      TEXT         NULL,
    host_id        TEXT         NOT NULL
                                REFERENCES sandbox.hosts(host_id) ON DELETE RESTRICT,
    -- Round-6 CAS counter; bumped on every ownership-relevant UPDATE
    -- (see § 6.3 CAS pattern + § 11 lease-based takeover).
    generation     BIGINT       NOT NULL DEFAULT 0
                                CHECK (generation >= 0),
    status         TEXT         NOT NULL DEFAULT 'starting'
                                CHECK (status IN ('starting', 'running', 'stopping',
                                                  'stopped', 'lost', 'recreating',
                                                  'orphan')),
    key_fp         TEXT         NOT NULL
                                CHECK (key_fp ~ '^[0-9a-f]{32}$'),
    created_at     TIMESTAMPTZ  NOT NULL DEFAULT now(),
    started_at     TIMESTAMPTZ  NULL,
    stopped_at     TIMESTAMPTZ  NULL,
    last_used_at   TIMESTAMPTZ  NOT NULL DEFAULT now(),
    deleted_at     TIMESTAMPTZ  NULL,
    metadata       JSONB        NOT NULL DEFAULT '{}'::JSONB
);

-- Partial unique: at most one active sandbox per (user_id, project_id).
-- 'recreating' is included because mid-recreate is still claiming the
-- slot (§ 6.3).
CREATE UNIQUE INDEX IF NOT EXISTS idx_sandboxes_active_user_project
    ON sandbox.sandboxes (user_id, project_id)
    WHERE deleted_at IS NULL AND status IN ('starting', 'running', 'recreating');

CREATE INDEX IF NOT EXISTS idx_sandboxes_user_id
    ON sandbox.sandboxes (user_id) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_sandboxes_host_id_status
    ON sandbox.sandboxes (host_id, status) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_sandboxes_status_last_used
    ON sandbox.sandboxes (status, last_used_at) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_sandboxes_created_at
    ON sandbox.sandboxes (created_at);

-- ─── 6.4 sandbox.shares ──────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS sandbox.shares (
    token_id        TEXT         PRIMARY KEY
                                 CHECK (token_id ~ '^tok_[0-9A-Za-z]{20,40}$'),
    sandbox_id      TEXT         NOT NULL
                                 REFERENCES sandbox.sandboxes(sandbox_id) ON DELETE CASCADE,
    port            SMALLINT     NOT NULL
                                 CHECK (port BETWEEN 1 AND 32767),
    scope           TEXT         NOT NULL
                                 CHECK (scope IN ('ro', 'rw')),
    secret_version  INTEGER      NOT NULL
                                 CHECK (secret_version >= 1),
    issued_at       TIMESTAMPTZ  NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ  NOT NULL,
    revoked_at      TIMESTAMPTZ  NULL,
    use_count       BIGINT       NOT NULL DEFAULT 0,
    last_used_at    TIMESTAMPTZ  NULL,
    iss             TEXT         NULL
                                 CHECK (iss IS NULL OR iss ~ '^usr_[0-9A-Za-z]{20,40}$'),
    deleted_at      TIMESTAMPTZ  NULL,
    CHECK (expires_at > issued_at),
    CHECK (revoked_at IS NULL OR revoked_at >= issued_at)
);

CREATE INDEX IF NOT EXISTS idx_shares_sandbox_id_port
    ON sandbox.shares (sandbox_id, port) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_shares_iss_issued_at
    ON sandbox.shares (iss, issued_at) WHERE deleted_at IS NULL AND iss IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_shares_expires_at
    ON sandbox.shares (expires_at) WHERE deleted_at IS NULL AND revoked_at IS NULL;

-- ─── 6.5 sandbox.events (PARTITION BY RANGE (ts)) ────────────────────

CREATE TABLE IF NOT EXISTS sandbox.events (
    event_id    TEXT         NOT NULL
                             CHECK (event_id ~ '^evt_[0-9A-Za-z]{20,40}$'),
    sandbox_id  TEXT         NOT NULL
                             CHECK (sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'),
    user_id     TEXT         NOT NULL
                             CHECK (user_id ~ '^usr_[0-9A-Za-z]{20,40}$'),
    kind        TEXT         NOT NULL,
    ts          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    data        JSONB        NOT NULL DEFAULT '{}'::JSONB
                             CHECK (pg_column_size(data) <= 8192),
    PRIMARY KEY (ts, event_id)
) PARTITION BY RANGE (ts);

-- Round-1 fix (C3): DEFAULT partition catches any INSERT outside the
-- pre-provisioned window so the live path never sees the
-- "no partition of relation" error.
CREATE TABLE IF NOT EXISTS sandbox.events_default
    PARTITION OF sandbox.events DEFAULT;

-- Six monthly partitions starting from the design's reference date
-- (2026-05). Today is 2026-05-04 so 2026-05 is the current month.
-- The controller-side `ensure_window` task (Phase 1+) provisions
-- forward partitions on a 1h cadence; this migration covers the
-- initial six-month window.
CREATE TABLE IF NOT EXISTS sandbox.events_2026_05
    PARTITION OF sandbox.events
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE IF NOT EXISTS sandbox.events_2026_06
    PARTITION OF sandbox.events
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE IF NOT EXISTS sandbox.events_2026_07
    PARTITION OF sandbox.events
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE IF NOT EXISTS sandbox.events_2026_08
    PARTITION OF sandbox.events
    FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE IF NOT EXISTS sandbox.events_2026_09
    PARTITION OF sandbox.events
    FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');
CREATE TABLE IF NOT EXISTS sandbox.events_2026_10
    PARTITION OF sandbox.events
    FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');

-- Round-4 index strategy (PERF-C1, PERF-C2): two BTREEs + BRIN + a
-- partial covering index on the metering hot kinds.
CREATE INDEX IF NOT EXISTS idx_events_user_id_ts
    ON sandbox.events (user_id, ts);
CREATE INDEX IF NOT EXISTS idx_events_sandbox_ts
    ON sandbox.events (sandbox_id, ts);
CREATE INDEX IF NOT EXISTS idx_events_ts_brin
    ON sandbox.events USING BRIN (ts) WITH (pages_per_range = 32);
CREATE INDEX IF NOT EXISTS idx_events_metering
    ON sandbox.events (ts)
    INCLUDE (sandbox_id, user_id, data)
    WHERE kind IN ('compute_seconds', 'share.used', 'preview_egress');

-- ─── 6.6 sandbox.deleted_sandboxes (tombstone) ───────────────────────

CREATE TABLE IF NOT EXISTS sandbox.deleted_sandboxes (
    sandbox_id   TEXT         PRIMARY KEY
                              CHECK (sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'),
    user_id      TEXT         NOT NULL
                              CHECK (user_id ~ '^usr_[0-9A-Za-z]{20,40}$'),
    deleted_at   TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_deleted_sandboxes_deleted_at
    ON sandbox.deleted_sandboxes (deleted_at);

-- ─── 13.2 Role split + grants (CI-permissive) ────────────────────────
--
-- In production the operator provisions the four roles via a separate
-- bootstrap script. In CI / cargo-test, the calling role usually
-- lacks CREATEROLE, so the DO block below tries to create the roles
-- and gracefully no-ops on `insufficient_privilege`. The grants run
-- only when the role exists; missing-role grants are skipped.

DO $bootstrap$
DECLARE
    can_create_role BOOLEAN;
BEGIN
    SELECT rolcreaterole INTO can_create_role
      FROM pg_roles
     WHERE rolname = current_user;

    IF can_create_role THEN
        IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_admin') THEN
            EXECUTE 'CREATE ROLE sandbox_admin NOLOGIN';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_app') THEN
            EXECUTE 'CREATE ROLE sandbox_app NOLOGIN';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
            EXECUTE 'CREATE ROLE sandbox_audit NOLOGIN';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_gdpr') THEN
            EXECUTE 'CREATE ROLE sandbox_gdpr NOLOGIN';
        END IF;
    END IF;

    -- DDL role (migrations).
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_admin') THEN
        EXECUTE 'GRANT CREATE, USAGE ON SCHEMA sandbox TO sandbox_admin';
    END IF;

    -- Runtime DML role.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_app') THEN
        EXECUTE 'GRANT USAGE ON SCHEMA sandbox TO sandbox_app';
        EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON
                    sandbox.sandboxes, sandbox.shares, sandbox.hosts,
                    sandbox.deleted_sandboxes, sandbox.schema_migrations
                    TO sandbox_app';
        -- Read-only on events: the controller cannot tamper with audit.
        EXECUTE 'GRANT SELECT ON sandbox.events TO sandbox_app';
    END IF;

    -- INSERT-only audit role.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        EXECUTE 'GRANT USAGE ON SCHEMA sandbox TO sandbox_audit';
        EXECUTE 'GRANT INSERT ON sandbox.events TO sandbox_audit';
    END IF;

    -- GDPR scoped DELETE role.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_gdpr') THEN
        EXECUTE 'GRANT USAGE ON SCHEMA sandbox TO sandbox_gdpr';
        EXECUTE 'GRANT SELECT, DELETE ON
                    sandbox.sandboxes, sandbox.shares, sandbox.events,
                    sandbox.deleted_sandboxes
                    TO sandbox_gdpr';
        EXECUTE 'GRANT INSERT ON sandbox.deleted_sandboxes, sandbox.events TO sandbox_gdpr';
    END IF;
EXCEPTION
    -- Restricted CI Postgres: silently degrade. The integration tests
    -- run the migration as a single role (typically `postgres`) and
    -- don't enforce role-isolation. Production verifies the four
    -- roles exist via a runbook check.
    WHEN insufficient_privilege THEN
        RAISE NOTICE 'sandbox role creation skipped: caller lacks privilege; production deploys roles separately';
END
$bootstrap$;
