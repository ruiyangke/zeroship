--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Metering export high-water ledger (M-Stripe, blueprint §M5/§M9): the
-- per-(app, period) last-exported compute-unit watermark for the pluggable
-- export providers (Stripe Billing Meters / OpenMeter).
-- ════════════════════════════════════════════════════════════════════════
--
-- The export cron (`cron::metering_export`, spawned ONLY for stripe/openmeter,
-- never native) forwards compute units to an external meter. Stripe meter
-- events are SUMMED on Stripe's side, so the cron must push the CU CONSUMED
-- SINCE THE LAST EXPORT — the DELTA — not the cumulative total, and must never
-- double-push on a cron re-run / crash.
--
-- `exported_units` is the DURABLE high-water mark: the cumulative CU already
-- pushed for `(app_id, period_start)`. Each sweep:
--   1. derives the app's CURRENT cumulative CU from `usage_aggregates` via
--      `pricing::total_units(weights, period_totals)` — NO CU column is added
--      anywhere; CU is always re-derived from the raw, re-weightable totals;
--   2. delta = current_total_units − exported_units;
--   3. pushes the delta (skips when 0 — the no-op that makes a re-run safe);
--   4. on a SUCCESSFUL push, UPDATEs `exported_units = current_total_units`.
--
-- The push `identifier` is deterministic from `(app, period, exported→current
-- window)` so a Stripe-side replay also dedups (defense in depth). The local
-- high-water is the PRIMARY guard: it prevents re-sending an already-exported
-- delta even if Stripe's dedupe window has expired.
--
-- This ledger is ADDITIVE and export-only: it NEVER feeds enforcement. The
-- spend cap (`spend.rs` / `enforce.rs`) reads `usage_aggregates` directly and
-- is provider-independent — the same local fact whether the provider is native,
-- stripe, or openmeter.
--
-- Pre-launch, no back-compat: additive DDL, no backfill.

-- ─── metering_exports (one row per (app, period); UPSERTed by the cron) ─────
--changeset zeroship:metering-exports splitStatements:true
CREATE TABLE zeroship.metering_exports (
    app_id         UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period_start   TIMESTAMPTZ NOT NULL,                       -- the period (UTC, first-of-month 00:00)
    exported_units BIGINT NOT NULL DEFAULT 0
                       CHECK (exported_units >= 0),            -- cumulative CU already pushed (monotonic)
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, period_start)                         -- one watermark per app per period
);
--rollback DROP TABLE zeroship.metering_exports;

-- ─── RLS (verbatim 0037/0039 fail-closed app_id-keyed pattern) ─────────────
-- ENABLE + FORCE + a single tenant_isolation USING policy on the app_id key
-- bound to the zeroship.tenant_app GUC. A request with the GUC unset reads
-- current_setting(...,true) => NULL, so `app_id = NULL` is NULL ⇒ no rows
-- (fail-closed). Control is BYPASSRLS so the export cron sweeps fleet-wide.
--changeset zeroship:metering-exports-rls splitStatements:true
ALTER TABLE zeroship.metering_exports ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.metering_exports FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.metering_exports
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.metering_exports;
--rollback ALTER TABLE zeroship.metering_exports NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.metering_exports DISABLE ROW LEVEL SECURITY;

-- ─── Least-priv grants to zeroship_control (guard on role existence) ───────
-- The export cron UPSERTs the high-water row (read current, push delta, write
-- back the new cumulative) ⇒ SELECT, INSERT, UPDATE. No DELETE — the watermark
-- is monotonic per period. Guard on role existence like 0037/0039 so a dev/test
-- DB without the 0025 role model (superuser-only) still migrates.
--changeset zeroship:metering-exports-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.metering_exports TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metering_exports FROM zeroship_control'; END IF; END $rb$;
