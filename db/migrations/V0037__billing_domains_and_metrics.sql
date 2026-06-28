-- period DATE pinned to first-of-month (kills the f64 round-trip in metering/mod.rs).
-- DOMAIN-over-CHECK keeps the wire types the bespoke compio-postgres driver already
-- round-trips (DATE / TEXT) — a native ENUM would force an enum codec for zero added safety.
CREATE DOMAIN zeroship.billing_period AS DATE
    CHECK (EXTRACT(DAY FROM VALUE) = 1);
-- spend states: membership only. Ordering (allow<warn<degrade<block) + transition
-- legality live in Rust (spend.rs severity()); the DB does NOT encode rank.
CREATE DOMAIN zeroship.spend_state AS TEXT
    CHECK (VALUE IN ('allow','warn','degrade','block'));
-- account (dunning) states: membership only; lifecycle lives in account_status.rs.
CREATE DOMAIN zeroship.account_state AS TEXT
    CHECK (VALUE IN ('active','past_due','suspended'));
-- metric provenance.
CREATE DOMAIN zeroship.metric_kind AS TEXT
    CHECK (VALUE IN ('platform','primitive','custom'));
-- invoice lifecycle. Period-claim short-circuit + immutability triggers key off this.
CREATE DOMAIN zeroship.invoice_status AS TEXT
    CHECK (VALUE IN ('draft','finalized','void'));

-- GLOBAL operator catalog (mirrors plans): NOT tenant-scoped, NO RLS (control is
-- BYPASSRLS). Platform/primitive rows seeded; custom SDK metrics auto-register on
-- first ingest (capped + GC'd) so usage_aggregates.metric FK always resolves.
CREATE TABLE zeroship.billing_metrics (
    metric       TEXT PRIMARY KEY,            -- 'requests','cpu_us',…, custom names
    kind         zeroship.metric_kind NOT NULL,
    unit         TEXT NOT NULL,               -- 'request','microsecond','byte' (display/audit)
    archived     BOOLEAN NOT NULL DEFAULT false,
    last_seen_at TIMESTAMPTZ,                 -- bumped on custom-metric ingest; drives GC
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Custom metrics must be attributable + cappable + GC-able. Platform/primitive rows
-- have NULL owner_app (global, never GC'd).
ALTER TABLE zeroship.billing_metrics
    ADD COLUMN owner_app UUID REFERENCES zeroship.apps(id) ON DELETE CASCADE;
CREATE INDEX billing_metrics_owner_app_idx ON zeroship.billing_metrics (owner_app)
    WHERE owner_app IS NOT NULL;

-- Mirrors the 0038 metric_weights seed names EXACTLY (parity ENFORCED by 0038's
-- seed-weight-parity-assert): every billable weight has a catalog parent; every
-- cataloged billable metric has a weight (no silent $0). Seeded here so the FKs in
-- 0038/0039 resolve at first boot regardless of seed_plans timing.
-- gateway_egress_bytes + stream_wall_us are the #27 metering-coverage metrics
-- (gateway own-egress + SSE/stream held-open wall) folded in from the former
-- 0047_metering_coverage_weights so the new usage_aggregates.metric → billing_metrics
-- FK (RESTRICT) and the 0038 seed-weight-parity-assert both stay green; their weights
-- live in the 0038 seed (gateway_egress_bytes 1/1000 like egress_bytes; stream_wall_us
-- 1/10000 like wall_us).
INSERT INTO zeroship.billing_metrics (metric, kind, unit) VALUES
    ('requests',            'platform',  'request'),
    ('cpu_us',              'platform',  'microsecond'),
    ('wall_us',             'platform',  'microsecond'),
    ('ingress_bytes',       'platform',  'byte'),
    ('egress_bytes',        'platform',  'byte'),
    ('gateway_egress_bytes','platform',  'byte'),
    ('stream_wall_us',      'platform',  'microsecond'),
    ('db_reads',            'primitive', 'op'),
    ('db_writes',           'primitive', 'op'),
    ('db_rows_written',     'primitive', 'row'),
    ('kv_reads',            'primitive', 'op'),
    ('kv_writes',           'primitive', 'op'),
    ('storage_ops',         'primitive', 'op'),
    ('storage_bytes',       'primitive', 'byte'),
    ('storage_egress_bytes','primitive', 'byte')
ON CONFLICT (metric) DO NOTHING;

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.billing_metrics TO zeroship_control';
  END IF;
END $g$;
