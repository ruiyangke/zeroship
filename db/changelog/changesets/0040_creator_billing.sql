--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Creator billing identity + per-period billing runs (billing PR6, ISS-31).
-- ════════════════════════════════════════════════════════════════════════
--
-- Stream-1 (infra-cost billing) creator identity. Distinct from the Stream-2
-- Connect `creator_accounts` (acct_…, changeset 0004): here the creator is a
-- platform Stripe CUSTOMER (cus_…) in the PLATFORM's own Stripe account, with a
-- saved PaymentMethod set via a Checkout setup-mode session. NO Connect, NO
-- application_fee — that is the separate Stream-2 epic.
--
-- `creator_id` is a USER id (mirrors creator_accounts.creator_id, which is
-- `REFERENCES zeroship.users(id)` in 0004): the reconciler resolves creator→app
-- ownership through `app_members WHERE role='owner'` (there is NO apps.creator_id
-- column — see changeset 0031), groups owned apps by user_id, and bills that
-- user_id as the creator.
--
-- `billing_runs` is the IDEMPOTENCY ledger: its PK (creator_id, period_start) is
-- claimed with `INSERT … ON CONFLICT DO NOTHING` BEFORE any Stripe call, so a
-- retried reconcile tick for an already-billed period is a pure no-op (no
-- double-bill). `stripe_invoice_id` stays NULL until the Stripe call succeeds;
-- the reconciler re-drives any row with a NULL invoice id (commit-then-crash
-- window) replaying the SAME deterministic Stripe Idempotency-Key.
--
-- Control-only bookkeeping keyed by creator_id (a user id, NOT an app id) ⇒ NO
-- per-tenant app RLS — mirrors usage_reports_seen / creator_accounts. Control is
-- BYPASSRLS regardless; the keys here are not app_ids.

--changeset zeroship:creator-billing splitStatements:true
CREATE TABLE zeroship.creator_billing (
    creator_id         UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE CASCADE,
    stripe_customer_id TEXT,                 -- cus_… (platform account); NULL until first billing/setup
    default_pm_set     BOOLEAN NOT NULL DEFAULT false,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE zeroship.billing_runs (
    creator_id        UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    period_start      TIMESTAMPTZ NOT NULL,  -- the billed month (UTC, first-of-month 00:00)
    amount_cents      BIGINT NOT NULL,
    stripe_invoice_id TEXT,                  -- NULL until the Stripe finalize call succeeds
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (creator_id, period_start)   -- the idempotency key (no double-bill per period)
);
--rollback DROP TABLE zeroship.billing_runs;
--rollback DROP TABLE zeroship.creator_billing;

--changeset zeroship:creator-billing-grants splitStatements:false
-- Least-privilege grants. billing_runs needs UPDATE for the stripe_invoice_id /
-- amount write-back after the Stripe call; creator_billing needs UPDATE for the
-- customer-id / default_pm_set upserts. No DELETE — bookkeeping is append/update
-- only.
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.creator_billing TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_runs    TO zeroship_control';
  END IF;
END $g$;
