--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Spend engine state (billing PR5, ISS-31): per-app derived spend-enforcement
-- state + an append-only transition history.
-- ════════════════════════════════════════════════════════════════════════
--
-- The control-plane spend-reconcile cron (~60s) prices each app's
-- current-period usage (usage_aggregates × the PR4 plan catalog) against its
-- EFFECTIVE spend limit (the creator override `spend_limit_cents` else the
-- plan default) and derives a SpendState { allow | warn | degrade | block }
-- with hysteresis. On a transition it UPSERTs app_spend_state and appends a
-- spend_state_history row. The gateway pulls the current state via
-- `registry.get_routes` (LEFT JOIN app_spend_state) onto RouteEntry.spend_state
-- and gates dispatch on it (Block → 402, Degrade → throttle).
--
-- eval_limit_cents persists the EFFECTIVE limit used at the last derive so the
-- engine can tell a genuine limit change (raised cap / plan change → immediate
-- recovery) from accrual oscillation near a fixed cap (deadband holds).
--
-- Pre-launch, no back-compat: additive DDL, no backfill.

-- ─── app_spend_state (one row per app; UPSERTed by the engine) ─────────────
--changeset zeroship:app-spend-state splitStatements:true
CREATE TABLE zeroship.app_spend_state (
    app_id            UUID PRIMARY KEY REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    spend_limit_cents BIGINT,                    -- creator OVERRIDE; NULL = use plan default
    eval_limit_cents  BIGINT NOT NULL DEFAULT 0, -- EFFECTIVE limit used at last derive_state
                                                 -- (override else plan default); the
                                                 -- raised-limit-recovery comparison reads this
    state             TEXT NOT NULL DEFAULT 'allow',
    spend_cents       BIGINT NOT NULL DEFAULT 0,
    period_start      TIMESTAMPTZ NOT NULL,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE zeroship.spend_state_history (
    app_id     UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    from_state TEXT NOT NULL, to_state TEXT NOT NULL,
    spend_cents BIGINT NOT NULL, limit_cents BIGINT,
    at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.spend_state_history;
--rollback DROP TABLE zeroship.app_spend_state;

-- ─── RLS (verbatim 0037/0025 fail-closed app_id-keyed pattern) ─────────────
-- ENABLE + FORCE + a single tenant_isolation USING policy on the app_id key
-- bound to the zeroship.tenant_app GUC. A request with the GUC unset reads
-- current_setting(...,true) => NULL, so `app_id = NULL` is NULL ⇒ no rows
-- (fail-closed). Control is BYPASSRLS so the engine still aggregates
-- fleet-wide. spend_state_history is append-only ⇒ no WITH CHECK is needed
-- beyond the USING predicate (control inserts under BYPASSRLS).
--changeset zeroship:app-spend-state-rls splitStatements:true
ALTER TABLE zeroship.app_spend_state    ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.app_spend_state    FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.app_spend_state
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
ALTER TABLE zeroship.spend_state_history ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.spend_state_history FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.spend_state_history
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.spend_state_history;
--rollback ALTER TABLE zeroship.spend_state_history NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.spend_state_history DISABLE ROW LEVEL SECURITY;
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.app_spend_state;
--rollback ALTER TABLE zeroship.app_spend_state NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.app_spend_state DISABLE ROW LEVEL SECURITY;

-- ─── Least-priv grants to zeroship_control (guard on role existence) ───────
-- The engine mutates app_spend_state (UPSERT) ⇒ SELECT,INSERT,UPDATE; history
-- is append-only ⇒ SELECT,INSERT. Guard on role existence like 0037/0026 so a
-- dev/test DB without the 0025 role model (superuser-only) still migrates.
--changeset zeroship:app-spend-state-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.app_spend_state    TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT         ON zeroship.spend_state_history TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.app_spend_state FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.spend_state_history FROM zeroship_control'; END IF; END $rb$;
