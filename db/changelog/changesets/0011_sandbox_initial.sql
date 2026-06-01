--liquibase formatted sql

-- pg-backed sandbox state (Phase 0). Transcribed verbatim from
-- crates/sandbox/migrations/0001_initial.sql into the unified `zeroship`
-- schema. Every `sandbox.<obj>` reference is rewritten to `zeroship.<obj>`.
--
-- Source-of-truth: docs/proposals/sandbox-pg-state.md § 6 (Schema). Round-7
-- design v8.
--
-- Consolidation deltas vs. the source migration:
--   * `CREATE SCHEMA IF NOT EXISTS sandbox` is DROPPED — the `zeroship`
--     schema already exists (0001_extensions_schemas.sql).
--   * `sandbox.schema_migrations` (the embedded-runner bookkeeping table)
--     is DROPPED, along with every grant that referenced it — Liquibase
--     tracks applied changesets via DATABASECHANGELOG now.
--   * The four roles (sandbox_admin / sandbox_app / sandbox_audit /
--     sandbox_gdpr), the partitioned events table + its partitions, all
--     CHECK constraints, indexes, and the role-split grant bundle are kept
--     faithfully (schema-qualified to `zeroship`).

-- ─── 6.2 zeroship.hosts ──────────────────────────────────────────────
--changeset zeroship-sandbox:sandbox-hosts splitStatements:true
CREATE TABLE zeroship.hosts (
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
CREATE INDEX idx_hosts_status_heartbeat
    ON zeroship.hosts (status, last_heartbeat);
CREATE INDEX idx_hosts_region_status
    ON zeroship.hosts (region, status);
--rollback DROP TABLE zeroship.hosts;

-- ─── 6.3 zeroship.sandboxes ──────────────────────────────────────────
--changeset zeroship-sandbox:sandbox-sandboxes splitStatements:true
CREATE TABLE zeroship.sandboxes (
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
                                REFERENCES zeroship.hosts(host_id) ON DELETE RESTRICT,
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
CREATE UNIQUE INDEX idx_sandboxes_active_user_project
    ON zeroship.sandboxes (user_id, project_id)
    WHERE deleted_at IS NULL AND status IN ('starting', 'running', 'recreating');
CREATE INDEX idx_sandboxes_user_id
    ON zeroship.sandboxes (user_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_sandboxes_host_id_status
    ON zeroship.sandboxes (host_id, status) WHERE deleted_at IS NULL;
CREATE INDEX idx_sandboxes_status_last_used
    ON zeroship.sandboxes (status, last_used_at) WHERE deleted_at IS NULL;
CREATE INDEX idx_sandboxes_created_at
    ON zeroship.sandboxes (created_at);
--rollback DROP TABLE zeroship.sandboxes;

-- ─── 6.4 zeroship.shares ─────────────────────────────────────────────
--changeset zeroship-sandbox:sandbox-shares splitStatements:true
CREATE TABLE zeroship.shares (
    token_id        TEXT         PRIMARY KEY
                                 CHECK (token_id ~ '^tok_[0-9A-Za-z]{20,40}$'),
    sandbox_id      TEXT         NOT NULL
                                 REFERENCES zeroship.sandboxes(sandbox_id) ON DELETE CASCADE,
    port            INTEGER      NOT NULL
                                 CHECK (port BETWEEN 1 AND 65535),
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
CREATE INDEX idx_shares_sandbox_id_port
    ON zeroship.shares (sandbox_id, port) WHERE deleted_at IS NULL;
CREATE INDEX idx_shares_iss_issued_at
    ON zeroship.shares (iss, issued_at) WHERE deleted_at IS NULL AND iss IS NOT NULL;
CREATE INDEX idx_shares_expires_at
    ON zeroship.shares (expires_at) WHERE deleted_at IS NULL AND revoked_at IS NULL;
--rollback DROP TABLE zeroship.shares;

-- ─── 6.5 zeroship.sandbox_events (PARTITION BY RANGE (ts)) ───────────
-- splitStatements:false: the partitioned parent + DEFAULT + monthly
-- partitions + indexes ship as one logical unit (mirrors how the
-- platform changesets group a table with its indexes).
--changeset zeroship-sandbox:sandbox-events splitStatements:false
CREATE TABLE zeroship.sandbox_events (
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
CREATE TABLE zeroship.sandbox_events_default
    PARTITION OF zeroship.sandbox_events DEFAULT;
-- Six monthly partitions starting from the design's reference date
-- (2026-05). The controller-side `ensure_window` task (Phase 1+)
-- provisions forward partitions on a 1h cadence; this changeset covers
-- the initial six-month window.
CREATE TABLE zeroship.sandbox_events_2026_05
    PARTITION OF zeroship.sandbox_events
    FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
CREATE TABLE zeroship.sandbox_events_2026_06
    PARTITION OF zeroship.sandbox_events
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
CREATE TABLE zeroship.sandbox_events_2026_07
    PARTITION OF zeroship.sandbox_events
    FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE zeroship.sandbox_events_2026_08
    PARTITION OF zeroship.sandbox_events
    FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE zeroship.sandbox_events_2026_09
    PARTITION OF zeroship.sandbox_events
    FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');
CREATE TABLE zeroship.sandbox_events_2026_10
    PARTITION OF zeroship.sandbox_events
    FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');
-- Round-4 index strategy (PERF-C1, PERF-C2): two BTREEs + BRIN + a
-- partial covering index on the metering hot kinds.
CREATE INDEX idx_sandbox_events_user_id_ts
    ON zeroship.sandbox_events (user_id, ts);
CREATE INDEX idx_sandbox_events_sandbox_ts
    ON zeroship.sandbox_events (sandbox_id, ts);
CREATE INDEX idx_sandbox_events_ts_brin
    ON zeroship.sandbox_events USING BRIN (ts) WITH (pages_per_range = 32);
CREATE INDEX idx_sandbox_events_metering
    ON zeroship.sandbox_events (ts)
    INCLUDE (sandbox_id, user_id, data)
    WHERE kind IN ('compute_seconds', 'share.used', 'preview_egress');
--rollback DROP TABLE zeroship.sandbox_events;

-- ─── 6.6 zeroship.deleted_sandboxes (tombstone) ──────────────────────
--changeset zeroship-sandbox:sandbox-deleted-sandboxes splitStatements:true
CREATE TABLE zeroship.deleted_sandboxes (
    sandbox_id   TEXT         PRIMARY KEY
                              CHECK (sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'),
    user_id      TEXT         NOT NULL
                              CHECK (user_id ~ '^usr_[0-9A-Za-z]{20,40}$'),
    deleted_at   TIMESTAMPTZ  NOT NULL DEFAULT now()
);
CREATE INDEX idx_deleted_sandboxes_deleted_at
    ON zeroship.deleted_sandboxes (deleted_at);
--rollback DROP TABLE zeroship.deleted_sandboxes;

-- ─── 13.2 Role split + grants (CI-permissive) ────────────────────────
--
-- The four sandbox roles (sandbox_admin / sandbox_app / sandbox_audit /
-- sandbox_gdpr) are created in a role-existence-guarded DO block so the
-- migration runs on restricted CI Postgres (where the calling role lacks
-- CREATEROLE). In production the operator deploys the four roles via a
-- separate bootstrap step; the DO block then no-ops because the roles
-- already exist. The grants run only when the role exists.
--
-- The `sandbox.schema_migrations` grant from the source migration is
-- DROPPED (the table no longer exists). splitStatements:false because the
-- DO block contains `;` inside `$$`.
--changeset zeroship-sandbox:sandbox-roles-grants splitStatements:false
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
        EXECUTE 'GRANT CREATE, USAGE ON SCHEMA zeroship TO sandbox_admin';
    END IF;

    -- Runtime DML role.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_app') THEN
        EXECUTE 'GRANT USAGE ON SCHEMA zeroship TO sandbox_app';
        EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON
                    zeroship.sandboxes, zeroship.shares, zeroship.hosts,
                    zeroship.deleted_sandboxes
                    TO sandbox_app';
        -- Read-only on events: the controller cannot tamper with audit.
        EXECUTE 'GRANT SELECT ON zeroship.sandbox_events TO sandbox_app';
    END IF;

    -- INSERT-only audit role.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit') THEN
        EXECUTE 'GRANT USAGE ON SCHEMA zeroship TO sandbox_audit';
        EXECUTE 'GRANT INSERT ON zeroship.sandbox_events TO sandbox_audit';
    END IF;

    -- GDPR scoped DELETE role.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_gdpr') THEN
        EXECUTE 'GRANT USAGE ON SCHEMA zeroship TO sandbox_gdpr';
        EXECUTE 'GRANT SELECT, DELETE ON
                    zeroship.sandboxes, zeroship.shares, zeroship.sandbox_events,
                    zeroship.deleted_sandboxes
                    TO sandbox_gdpr';
        EXECUTE 'GRANT INSERT ON zeroship.deleted_sandboxes, zeroship.sandbox_events TO sandbox_gdpr';
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
--rollback SELECT 1;
