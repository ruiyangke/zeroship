--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Plan catalog: the operator-editable, server-side pricing catalog
-- (billing PR4, closes CT-A1 — free-text plan_id self-escalation).
-- ════════════════════════════════════════════════════════════════════════
--
-- Each plan is a row keyed by a `pln_<base62>` typed id. `apps.plan_id`
-- becomes an FK into it, so an app can no longer pick an unpriced or
-- oversized plan. The price model, included quota, and runtime limits are
-- JSONB columns deserialized into the pure Rust types
-- (`crate::pricing::{PlanPrice, PricingRule}` + `AppRuntimeLimits`).
--
-- `plans` is a GLOBAL operator catalog — NOT tenant-scoped — so it has NO
-- RLS (control is BYPASSRLS). Grants SELECT,INSERT,UPDATE to
-- zeroship_control (no DELETE — plans are archived, never hard-deleted, so
-- historical billing_runs + existing apps.plan_id FKs stay resolvable).
--
-- Pre-launch, no back-compat: the built-in tiers are seeded with real
-- `pln_…` ids by the control bootstrap (`seed_plans()` in
-- bootstrap_console.rs), which runs BEFORE the console-app upsert so the
-- new FK is satisfied at boot.

--changeset zeroship:plan-catalog splitStatements:true
CREATE TABLE zeroship.plans (
    id                        TEXT        PRIMARY KEY,          -- pln_<base62>
    name                      TEXT        NOT NULL,
    base_fee_cents            BIGINT      NOT NULL DEFAULT 0,
    price_model_json          JSONB       NOT NULL,             -- {metric: PricingRule}
    included_quota_json       JSONB       NOT NULL DEFAULT '{}',-- {metric: u64}
    runtime_limits_json       JSONB       NOT NULL,             -- AppRuntimeLimits
    spend_limit_default_cents BIGINT      NOT NULL DEFAULT 0,
    archived                  BOOLEAN     NOT NULL DEFAULT false,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
ALTER TABLE zeroship.apps
    ADD CONSTRAINT apps_plan_fk FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id);
--rollback ALTER TABLE zeroship.apps DROP CONSTRAINT apps_plan_fk;
--rollback DROP TABLE zeroship.plans;

--changeset zeroship:plan-catalog-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.plans TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.plans FROM zeroship_control'; END IF; END $rb$;
