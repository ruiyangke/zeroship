-- The LOCAL, fast, provider-independent enforcement fact. Single-row-per-
-- (app,period,metric) KEPT — slot=worker_id sharding is the future answer to write
-- contention but there is no measured contention pre-launch.
CREATE TABLE zeroship.usage_aggregates (
    app_id     UUID                    NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period     zeroship.billing_period NOT NULL,
    -- Schema MAJOR-1(ii): NO ACTION DEFERRABLE INITIALLY DEFERRED, not immediate
    -- RESTRICT. A custom metric is `billing_metrics.owner_app → apps ON DELETE
    -- CASCADE` (0037); deleting an app CASCADE-deletes both this app's
    -- usage_aggregates rows (via app_id) AND its custom billing_metrics rows. With
    -- an IMMEDIATE RESTRICT here, the metric-delete would abort the moment the
    -- still-present aggregate row references it (order-dependent). Deferring the
    -- check to end-of-statement lets the app_id CASCADE remove the aggregate rows
    -- FIRST, so by commit the metric FK is satisfied (no dangling aggregate metric)
    -- AND a concurrent app-delete succeeds. The "no aggregate without a cataloged
    -- metric" guarantee is preserved — it is just checked at statement end.
    metric     TEXT                    NOT NULL REFERENCES zeroship.billing_metrics(metric)
                                       ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED,
    total      BIGINT                  NOT NULL DEFAULT 0 CHECK (total >= 0),
    updated_at TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, period, metric)
);
-- The spend/export sweeps read the whole fleet's CURRENT period (WHERE period=$1).
-- PK leads with app_id, so add the period-leading index.
CREATE INDEX usage_aggregates_period_idx ON zeroship.usage_aggregates (period, app_id);

ALTER TABLE zeroship.usage_aggregates ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.usage_aggregates FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.usage_aggregates
    USING      (app_id = current_setting('zeroship.tenant_app', true)::uuid)
    WITH CHECK (app_id = current_setting('zeroship.tenant_app', true)::uuid);

-- IDEMPOTENCY INVARIANT: the dedup gate is `INSERT … (worker_id, sequence) ON CONFLICT
-- DO NOTHING`; the period is derived AT INGEST TIME, not carried on the report.
-- `period` MUST NOT be in the dedup PRIMARY KEY: a retried report straddling the UTC
-- month boundary would hash to a new key, pass the gate, and DOUBLE-APPLY.
-- POSTURE: `period` is present + NULLable but is NOT WRITTEN at ingest today. The hot
-- ingest INSERT stays EXACTLY `(worker_id, sequence)` — NO third bind, ZERO write-
-- amplification. The column + its index are introduced for retention; the retention-
-- activation PR (the ONLY consumer) is what starts populating it and adds the period
-- index in the SAME PR. Until then the column is inert.
CREATE TABLE zeroship.usage_reports_seen (
    worker_id TEXT                    NOT NULL,
    sequence  BIGINT                  NOT NULL,
    period    zeroship.billing_period,           -- NULLable; retention-only; NOT written at ingest
    seen_at   TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    PRIMARY KEY (worker_id, sequence)
);
-- (NO period index here. Added by the retention-activation PR alongside the cron.)

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.usage_aggregates   TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, DELETE         ON zeroship.usage_reports_seen TO zeroship_control';
  END IF;
END $g$;
