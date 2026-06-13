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
    amount_cents      BIGINT NOT NULL CHECK (amount_cents >= 0),  -- money is non-negative (MINOR-19)
    stripe_invoice_id TEXT,                  -- NULL until the Stripe finalize call succeeds
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (creator_id, period_start)   -- the idempotency key (no double-bill per period)
);
-- Per-app invoice-item ledger (CRIT-1). The `billing_runs` PK claims that a
-- period was *claimed*, but NOT which apps' invoice items already posted to
-- Stripe. On a re-drive after a crash/timeout — and after Stripe's 24h
-- Idempotency-Key window has expired — replaying the same key no longer dedupes,
-- so without this ledger an already-posted app's item would post a SECOND time
-- (double-bill). We record one row here the instant `create_invoice_item`
-- returns; on (re-)drive we skip any app already present, so each app's item is
-- posted AT MOST ONCE regardless of Stripe key expiry. The ledger — not Stripe's
-- 24h key — is the durable double-bill guard.
CREATE TABLE zeroship.billing_run_items (
    creator_id     UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    period_start   TIMESTAMPTZ NOT NULL,  -- matches billing_runs.period_start
    app_id         UUID NOT NULL,
    stripe_item_id TEXT NOT NULL,         -- the ii_… Stripe returned
    amount_cents   BIGINT NOT NULL CHECK (amount_cents >= 0),  -- money is non-negative (MINOR-19)
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (creator_id, period_start, app_id)  -- one posted item per app per period
);
--rollback DROP TABLE zeroship.billing_run_items;
--rollback DROP TABLE zeroship.billing_runs;
--rollback DROP TABLE zeroship.creator_billing;

--changeset zeroship:creator-billing-grants splitStatements:false
-- Least-privilege grants. billing_runs needs UPDATE for the stripe_invoice_id /
-- amount write-back after the Stripe call; creator_billing needs UPDATE for the
-- customer-id / default_pm_set upserts. No DELETE — bookkeeping is append/update
-- only.
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.creator_billing  TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_runs      TO zeroship_control';
    -- The per-app ledger is append-only (INSERT) + SELECT on (re-)drive.
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.billing_run_items         TO zeroship_control';
  END IF;
END $g$;

--changeset zeroship:billing-owner-index splitStatements:true
-- The reconciler's creator→app owner sweep scans `app_members WHERE
-- role='owner'`. A partial index on the owner rows keyed by user_id makes that
-- per-tick scan an index range read instead of a seq scan as membership grows
-- (MINOR-8 / MINOR-20).
CREATE INDEX IF NOT EXISTS app_members_owner_by_user_idx
    ON zeroship.app_members (user_id)
    WHERE role = 'owner';
--rollback DROP INDEX IF EXISTS zeroship.app_members_owner_by_user_idx;
