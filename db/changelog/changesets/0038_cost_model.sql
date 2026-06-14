--liquibase formatted sql

--changeset zeroship:metric-weights splitStatements:true
-- CU stays DERIVED from raw totals at pricing time (no CU column). Reproducibility
-- for FINALIZED periods comes from snapshot-onto-line, so weights/FX need NOT be
-- versioned.
CREATE TABLE zeroship.metric_weights (
    metric       TEXT        PRIMARY KEY REFERENCES zeroship.billing_metrics(metric) ON DELETE RESTRICT,
    units_per_op BIGINT      NOT NULL CHECK (units_per_op >= 0),
    per_units    BIGINT      NOT NULL CHECK (per_units > 0),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE zeroship.pricing_config (
    id                     TEXT        PRIMARY KEY DEFAULT 'global' CHECK (id = 'global'),
    fx_pico_cents_per_unit BIGINT      NOT NULL CHECK (fx_pico_cents_per_unit >= 1000),  -- floor = MIN_FX_PICO_CENTS_PER_UNIT
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.pricing_config;
--rollback DROP TABLE zeroship.metric_weights;

--changeset zeroship:metric-weights-seed splitStatements:true
-- gateway_egress_bytes + stream_wall_us are the #27 metering-coverage weights folded
-- in from the former 0047_metering_coverage_weights — weighted IDENTICALLY to their
-- worker-owned twins (gateway_egress_bytes 1 CU / 1000 B like egress_bytes;
-- stream_wall_us 1 CU / 10 ms like wall_us) so the gateway/stream billing #27 wired
-- keeps pricing through the unchanged CU→FX pipeline and the parity assert below stays
-- green. Their catalog rows live in the 0037 billing_metrics seed.
INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) VALUES
    ('requests',            1, 1),
    ('cpu_us',              1, 1000),
    ('wall_us',             1, 10000),
    ('ingress_bytes',       1, 10000),
    ('egress_bytes',        1, 1000),
    ('gateway_egress_bytes',1, 1000),
    ('stream_wall_us',      1, 10000),
    ('db_reads',            1, 1),
    ('db_writes',           2, 1),
    ('db_rows_written',     1, 50),
    ('kv_reads',            1, 1),
    ('kv_writes',           2, 1),
    ('storage_ops',         2, 1),
    ('storage_bytes',       1, 1000),
    ('storage_egress_bytes',1, 1000)
ON CONFLICT (metric) DO NOTHING;
INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit) VALUES ('global', 30000000)
ON CONFLICT (id) DO NOTHING;
--rollback DELETE FROM zeroship.pricing_config WHERE id = 'global';
--rollback DELETE FROM zeroship.metric_weights;

--changeset zeroship:seed-weight-parity-assert splitStatements:false
-- ENFORCE "no silent $0" at migration time: every non-custom cataloged metric MUST
-- have a weight (the FK gives the other direction). Fails the migration loudly if the
-- two hand-maintained seed lists diverge.
DO $assert$
DECLARE missing TEXT;
BEGIN
    SELECT string_agg(m.metric, ', ') INTO missing
    FROM zeroship.billing_metrics m
    LEFT JOIN zeroship.metric_weights w ON w.metric = m.metric
    WHERE m.kind IN ('platform','primitive') AND m.archived = false AND w.metric IS NULL;
    IF missing IS NOT NULL THEN
        RAISE EXCEPTION 'billing seed parity: cataloged billable metric(s) without a weight (silent $0): %', missing;
    END IF;
END
$assert$;
--rollback SELECT 1;

--changeset zeroship:compute-unit-pricing-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.metric_weights TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.pricing_config TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metric_weights FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.pricing_config FROM zeroship_control'; END IF; END $rb$;
