--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Creator fee policy + Connect onboarding verification signals
-- (billing G1, Stream-2: Connect + server-stamped application fee).
-- ════════════════════════════════════════════════════════════════════════
--
-- Stream-2 is the CREATOR-REVENUE rail: a creator charges THEIR end-users via
-- their own Stripe **Connect** account (acct_…, changeset 0004). The platform
-- takes a per-creator fee, stamped on the Connect charge SERVER-SIDE.
--
-- The fee was previously chosen CLIENT-SIDE by the `@zeroship/payments` SDK
-- (`applicationFeePercent ?? 15`) — fully bypassable: any creator could set
-- their own fee to 0 (ISS-29). The fix is a SERVER-HELD `creator_fee_policy`
-- this changeset adds: the fee lives in the DB, is OPERATOR-ONLY to write (no
-- creator-reachable path to lower it), and the checkout endpoint resolves +
-- stamps it server-side so creator code can neither read nor set it.
--
-- `creator_fee_policy` is keyed by `creator_id` (a USER id, mirrors
-- `creator_billing`/`creator_billing_status`), NOT an app id ⇒ NO per-tenant
-- app RLS — it is OPERATOR CONFIG; control is BYPASSRLS and the gateway never
-- touches it. The DEFAULT for a creator with no row is `percent 15%` resolved
-- in code (no row inserted at signup) so a brand-new creator still pays 15%.
--
-- This changeset ALSO extends `creator_accounts` with the Connect onboarding
-- verification signals (`charges_enabled`/`payouts_enabled`/`details_submitted`)
-- the `callback` handler writes from a server-side `retrieve_account` — closing
-- the "callback trusts a POSTed acct_…" hole (ISS-30): the acct_… is fetched
-- from Stripe and matched to what we created, never trusted from the client body.
--
-- Pre-launch, no back-compat: additive DDL, no backfill (no production data).

-- ─── creator_fee_policy (one row per creator; OPERATOR-only writes) ─────────
--changeset zeroship:creator-fee-policy splitStatements:true
CREATE TABLE zeroship.creator_fee_policy (
    creator_id   UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE CASCADE,
    -- 'fixed'   → a flat `amount_cents` per charge.
    -- 'percent' → `percent_bps` basis points of the txn (1500 = 15%), optionally
    --             clamped to `[floor_cents, cap_cents]`.
    kind         TEXT   NOT NULL CHECK (kind IN ('fixed','percent')),
    amount_cents BIGINT,                         -- kind='fixed' (non-negative)
    percent_bps  INT,                            -- kind='percent', basis points
    cap_cents    BIGINT,                         -- optional upper clamp
    floor_cents  BIGINT,                         -- optional lower clamp
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Shape integrity: a fixed policy carries a non-negative amount; a percent
    -- policy carries bps in [0, 10000] (0%..100%). Money is never negative.
    CONSTRAINT creator_fee_policy_shape CHECK (
            (kind = 'fixed'   AND amount_cents IS NOT NULL AND amount_cents >= 0)
         OR (kind = 'percent' AND percent_bps  IS NOT NULL AND percent_bps BETWEEN 0 AND 10000)
    ),
    CONSTRAINT creator_fee_policy_cap_nonneg   CHECK (cap_cents   IS NULL OR cap_cents   >= 0),
    CONSTRAINT creator_fee_policy_floor_nonneg CHECK (floor_cents IS NULL OR floor_cents >= 0)
);
--rollback DROP TABLE zeroship.creator_fee_policy;

-- ─── creator_accounts Connect verification signals (ISS-30 ownership) ───────
-- The `callback` handler retrieves the account from Stripe and writes these,
-- so the gateway/dashboard can tell a fully-onboarded Connect account from a
-- pending one. ALTER is acceptable pre-launch — these are the Connect-side
-- tables, NOT the redesign's infra-billing tables.
--changeset zeroship:creator-accounts-connect-flags splitStatements:true
ALTER TABLE zeroship.creator_accounts
    ADD COLUMN charges_enabled   BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN payouts_enabled   BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN details_submitted BOOLEAN NOT NULL DEFAULT false;
--rollback ALTER TABLE zeroship.creator_accounts DROP COLUMN details_submitted, DROP COLUMN payouts_enabled, DROP COLUMN charges_enabled;

-- ─── Grants (control is the only writer; OPERATOR-gated at the handler) ─────
-- The operator endpoint UPSERTs the fee policy (SELECT,INSERT,UPDATE); the
-- checkout path SELECTs it. No DELETE — operator config is upsert-only (clearing
-- a policy reverts to the in-code 15% default by leaving the row absent, but the
-- operator path never deletes). Guard on role existence like 0040/0045 so a
-- dev/test DB without the 0025 role model (superuser-only) still migrates.
--changeset zeroship:creator-fee-policy-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.creator_fee_policy TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.creator_fee_policy FROM zeroship_control'; END IF; END $rb$;
