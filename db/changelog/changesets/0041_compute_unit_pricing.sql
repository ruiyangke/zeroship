--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Compute-unit pricing (billing-v2 Refactor B): the GLOBAL cost model
-- (`metric_weights`) + the global default FX (`pricing_config`).
-- ════════════════════════════════════════════════════════════════════════
--
-- The cost model — "how many compute units (CU) a metric op costs" — is a
-- fleet-wide, operator-editable table (`metric_weights`). The price lever —
-- "how many cents one CU sells for" (the FX) — has a global default in the
-- single-row `pricing_config`; each plan may override it
-- (`plans.fx_pico_cents_per_unit`, NULL ⇒ this default).
--
-- FX is stored as an integer **pico-cents per CU** (10^-12 cent) so a sub-cent
-- unit price is representable without floats; the `× fx ÷ 10^12` conversion is
-- done once, half-up, at the pricing boundary (`crate::pricing::charge_cents`).
--
-- A metric weight is `units_per_op` CU per `per_units` ops, so a sub-unit
-- weight is exact (1 CU per 1000 egress_bytes ⇒ units_per_op=1, per_units=1000).
--
-- Both tables are GLOBAL operator config (mirror `plans`): NOT tenant-scoped,
-- NO RLS (control is BYPASSRLS), least-priv grants to zeroship_control.
-- usage_aggregates (0037) is UNCHANGED — raw-metric-keyed; CU are derived at
-- pricing time so a re-weight reprices history without touching stored usage.

--changeset zeroship:metric-weights splitStatements:true
CREATE TABLE zeroship.metric_weights (
    metric        TEXT        PRIMARY KEY,
    -- units_per_op >= 0 (MINOR-2): a negative weight is meaningless (it would
    -- credit CU). The loader coerces defensively + warns, but the CHECK makes a
    -- negative weight unrepresentable at the source.
    units_per_op  BIGINT      NOT NULL CHECK (units_per_op >= 0), -- CU per per_units ops
    per_units     BIGINT      NOT NULL CHECK (per_units > 0),     -- divisor (sub-unit weights)
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Single-row global pricing config. `id` is a fixed sentinel (always 'global')
-- with a CHECK so the table can only ever hold the one row.
CREATE TABLE zeroship.pricing_config (
    id                       TEXT        PRIMARY KEY DEFAULT 'global' CHECK (id = 'global'),
    fx_pico_cents_per_unit   BIGINT      NOT NULL,            -- default cents-per-CU @ 10^-12 scale
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.pricing_config;
--rollback DROP TABLE zeroship.metric_weights;

--changeset zeroship:metric-weights-seed splitStatements:true
-- Seed the platform-counter weights + the db/kv/storage weights (Refactor A
-- emits these later; harmless until then — an unweighted metric is free, a
-- weighted-but-unemitted metric simply never accrues). Weights chosen so the
-- 5 platform counters dominate and bytes/cpu cost sub-unit:
--   requests       1 CU / request
--   cpu_us         1 CU / 1000 cpu-microseconds  (1 CU per ms of CPU)
--   wall_us        1 CU / 10000 wall-microseconds (wall is cheaper than CPU)
--   ingress_bytes  1 CU / 10000 bytes
--   egress_bytes   1 CU / 1000 bytes              (egress costs more than ingress)
--   db_reads/db_writes/db_rows_*   op/row weights (Refactor A)
--   kv_reads/kv_writes             op weights
--   storage_ops/storage_bytes/storage_egress_bytes  op + byte weights
INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) VALUES
    ('requests',            1, 1),
    ('cpu_us',              1, 1000),
    ('wall_us',             1, 10000),
    ('ingress_bytes',       1, 10000),
    ('egress_bytes',        1, 1000),
    ('db_reads',            1, 1),
    ('db_writes',           2, 1),
    ('db_rows_read',        1, 100),
    ('db_rows_written',     1, 50),
    ('kv_reads',            1, 1),
    ('kv_writes',           2, 1),
    ('storage_ops',         2, 1),
    ('storage_bytes',       1, 1000),
    ('storage_egress_bytes',1, 1000)
ON CONFLICT (metric) DO NOTHING;

-- Global default FX: 0.00003 cent per CU. FX is pico-cents/CU (10^-12 cent), so
-- 0.00003 cent = 3e-5 cent = 3e-5 × 1e12 = 30_000_000 pico-cents/CU. With 1 CU
-- = 1 request this reproduces the historical "$0.30 per 1,000,000 requests"
-- rate (1e6 CU × 0.00003c = 30c), now applied uniformly across all CU.
INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit) VALUES
    ('global', 30000000)
ON CONFLICT (id) DO NOTHING;
--rollback DELETE FROM zeroship.pricing_config WHERE id = 'global';
--rollback DELETE FROM zeroship.metric_weights;

--changeset zeroship:compute-unit-pricing-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.metric_weights TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.pricing_config TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metric_weights FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.pricing_config FROM zeroship_control'; END IF; END $rb$;
