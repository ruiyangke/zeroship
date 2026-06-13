--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Metering pipeline: per-(app, billing-period, metric) usage aggregates +
-- idempotent at-least-once report dedup.
-- ════════════════════════════════════════════════════════════════════════
--
-- The worker meter flushes a `UsageReport { worker_id, report_id, sequence,
-- counters: { app_id → AppUsage } }` to control every ~10s, at-least-once
-- (a timed-out POST is retried next tick). Control ingests idempotently:
--
--   1. `usage_reports_seen(worker_id, sequence)` — the dedup ledger. An
--      INSERT … ON CONFLICT DO NOTHING that affects 0 rows means "already
--      seen" → the whole report is a no-op (no double-count). The PRIMARY
--      KEY (worker_id, sequence) is the dedup key.
--   2. `usage_aggregates(app_id, period_start, metric, total)` — the running
--      totals, keyed by calendar-month period_start (UTC, 00:00:00 on the
--      1st). Ingest UPSERTs `total = total + delta` per metric. A new month
--      lands in a new (period_start) row automatically.
--
-- The five fixed platform counters (requests, cpu_us, wall_us, egress_bytes,
-- ingress_bytes) and every SDK-defined `custom` metric share the same
-- (metric TEXT) row shape — no per-metric DDL.
--
-- Pre-launch, no back-compat: additive DDL, no backfill (no production data).

-- ─── usage_aggregates ─────────────────────────────────────────────────────
--changeset zeroship:metering-usage-aggregates splitStatements:true
CREATE TABLE zeroship.usage_aggregates (
    app_id       UUID        NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    -- Calendar-month boundary, 00:00:00 UTC on the 1st. The aggregation period.
    period_start TIMESTAMPTZ NOT NULL,
    -- One of the five fixed counters or an SDK `custom` metric name.
    metric       TEXT        NOT NULL,
    total        BIGINT      NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, period_start, metric)
);
--rollback DROP TABLE zeroship.usage_aggregates;

-- ─── usage_reports_seen (idempotent dedup ledger) ──────────────────────────
-- worker_id is free-text (the worker's identity string), sequence is the
-- monotonic per-worker counter. Together they dedup at-least-once retries.
--changeset zeroship:metering-usage-reports-seen splitStatements:true
CREATE TABLE zeroship.usage_reports_seen (
    worker_id TEXT        NOT NULL,
    sequence  BIGINT      NOT NULL,
    seen_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (worker_id, sequence)
);
--rollback DROP TABLE zeroship.usage_reports_seen;

-- ─── Grants (the 0025 role model is real; control is the only writer) ──────
-- control ingests reports: INSERT+UPDATE (UPSERT) on usage_aggregates,
-- INSERT (dedup) + SELECT (high-water resync) on usage_reports_seen.
-- splitStatements:false: a DO block carries `;` inside the body.
--changeset zeroship:metering-grants splitStatements:false
DO $grants$
BEGIN
    -- Guard on role existence exactly like 0026/sandbox: in a dev/test DB
    -- created without the 0025 role model (superuser-only), these roles may
    -- not exist — skip the grants rather than abort the migration.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
        EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.usage_aggregates  TO zeroship_control';
        EXECUTE 'GRANT SELECT, INSERT         ON zeroship.usage_reports_seen TO zeroship_control';
    END IF;
END
$grants$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.usage_aggregates FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.usage_reports_seen FROM zeroship_control'; END IF; END $rb$;

-- ─── RLS: usage_aggregates (key: app_id UUID; GUC zeroship.tenant_app) ─────
-- Consistent with the four 0025 tenant tables: ENABLE + FORCE RLS, one
-- tenant_isolation policy keyed on the per-request `zeroship.tenant_app`
-- GUC. Fails CLOSED (unset GUC → NULL predicate → zero rows). control is
-- BYPASSRLS (it aggregates fleet-wide on ingest), so this confines any
-- future non-bypass reader (a creator-facing usage read on the gateway
-- role) to its own app's rows. usage_reports_seen is NOT app-keyed (it is
-- worker-keyed control-internal bookkeeping), so it gets no tenant policy.
--changeset zeroship:metering-rls-usage-aggregates splitStatements:true
ALTER TABLE zeroship.usage_aggregates ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.usage_aggregates FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.usage_aggregates
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.usage_aggregates;
--rollback ALTER TABLE zeroship.usage_aggregates NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.usage_aggregates DISABLE ROW LEVEL SECURITY;
