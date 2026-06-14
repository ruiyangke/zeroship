--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Creator self-service plan assignment guardrail (billing-v2 MAJOR-4).
-- ════════════════════════════════════════════════════════════════════════
--
-- Before this change `PUT /api/apps/:id/plan` was gated only by
-- BillingWrite/Resource::App — which an app_owner satisfies — and the registry
-- only checked the plan exists + is unarchived. So a creator could self-assign
-- a CHEAPER operator plan (e.g. console/unlimited) and underpay. This is
-- asymmetric with the reduction-only spend-limit override.
--
-- The fix is self-service WITH a guardrail (not operator-only): a new
-- `assignable_by_creator` flag marks which tiers a creator may pick. The
-- public tiers (`free`, `pro`) are creator-assignable; the
-- console/enterprise/unlimited tiers are NOT. The HTTP `set_plan` handler gates
-- an app_owner principal to plans with `assignable_by_creator = true`; an
-- operator (BillingWrite on Resource::Any) may assign ANY plan. Operators
-- control the flag through the catalog upsert.
--
-- New rows default to FALSE (fail-closed: an operator-minted plan is NOT
-- creator-assignable unless explicitly marked). The built-in `free`/`pro` tiers
-- are flipped to TRUE by the control bootstrap `seed_plans()` (which carries the
-- per-tier intent) on every boot; the deterministic `pln_<base62>` ids are not
-- known at SQL-authoring time, so the seed — not this DDL — sets the public
-- tiers true.

--changeset zeroship:plan-assignable-by-creator splitStatements:true
ALTER TABLE zeroship.plans
    ADD COLUMN assignable_by_creator BOOLEAN NOT NULL DEFAULT false;
--rollback ALTER TABLE zeroship.plans DROP COLUMN assignable_by_creator;
