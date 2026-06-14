--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Creator payment/account status (billing G2): payment-failure → dunning →
-- suspension state, enforced at the gateway.
-- ════════════════════════════════════════════════════════════════════════
--
-- The spend engine (changeset 0039) caps USAGE within a paid relationship.
-- This table is the orthogonal ACCOUNT-level gate: a creator whose infra
-- invoice cannot be charged accrues unbounded cost. We drive a failed-payment
-- lifecycle off Stripe webhook truth and let the gateway stop serving a
-- creator's apps on non-payment.
--
--   active ──invoice.payment_failed──► past_due ──dunning exhausted──► suspended
--      ▲                                   │                                │
--      └──────── invoice.paid ─────────────┴────────── invoice.paid ────────┘
--
-- * active    — current; served (subject to spend).
-- * past_due  — ≥1 invoice failed; Stripe's own retries still running; the
--               customer-favourable GRACE window (still served).
-- * suspended — dunning window elapsed (`max_dunning_days`, default 7); the
--               gateway returns 402 ACCOUNT_SUSPENDED before dispatch. REVERSIBLE:
--               a later `invoice.paid` clears it straight back to active.
--
-- Keyed by `creator_id` (a USER id, mirrors `creator_billing` from changeset
-- 0040), NOT an app id ⇒ NO per-tenant app RLS — control is BYPASSRLS and the
-- key is not an app_id. The gateway projects this creator-keyed state onto each
-- of the creator's apps via the `app_members(role='owner')` join in
-- `registry.get_routes` (exactly like spend_state, but resolved creator→app).
--
-- Pre-launch, no back-compat: additive DDL, no backfill.

-- ─── creator_billing_status (one row per creator; UPSERTed by the webhook/cron) ─
--changeset zeroship:creator-billing-status splitStatements:true
CREATE TABLE zeroship.creator_billing_status (
    creator_id            UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE CASCADE,
    state                 TEXT NOT NULL DEFAULT 'active'
                              CHECK (state IN ('active','past_due','suspended')),
    -- Start of the CURRENT dunning window (set on the active→past_due edge;
    -- cleared back to NULL on recovery). The dunning cron measures
    -- `NOW() - past_due_since` against `max_dunning_days` to suspend.
    past_due_since        TIMESTAMPTZ,
    -- When the creator was suspended (audit / dashboard). NULL unless suspended.
    suspended_at          TIMESTAMPTZ,
    -- Most recent failed-invoice signal — the guard for idempotent
    -- payment_failed handling (a re-delivered event for the SAME invoice does
    -- not reset the dunning clock).
    last_payment_failure_at TIMESTAMPTZ,
    failed_invoice_id     TEXT,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- Append-only transition history (mirrors spend_state_history): every
-- account-state edge writes one row so ops can answer "when/why was this
-- creator suspended / recovered."
CREATE TABLE zeroship.creator_billing_status_history (
    creator_id UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    from_state TEXT NOT NULL, to_state TEXT NOT NULL,
    reason     TEXT,                       -- 'payment_failed' | 'dunning_exhausted' | 'payment_recovered'
    at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- Per-creator history read (newest-first) must not seq-scan the unbounded
-- append-only table.
CREATE INDEX idx_creator_billing_status_history_creator_at
    ON zeroship.creator_billing_status_history (creator_id, at DESC);
-- The dunning cron scans for past_due rows whose window is exhausted. A partial
-- index on the past_due rows keyed by `past_due_since` makes that per-tick scan
-- an index read rather than a seq scan over every creator.
CREATE INDEX idx_creator_billing_status_past_due
    ON zeroship.creator_billing_status (past_due_since)
    WHERE state = 'past_due';
--rollback DROP INDEX IF EXISTS zeroship.idx_creator_billing_status_past_due;
--rollback DROP INDEX IF EXISTS zeroship.idx_creator_billing_status_history_creator_at;
--rollback DROP TABLE zeroship.creator_billing_status_history;
--rollback DROP TABLE zeroship.creator_billing_status;

--changeset zeroship:creator-billing-status-grants splitStatements:false
-- Least-privilege grants. The webhook + dunning cron UPSERT the status row
-- (SELECT,INSERT,UPDATE); history is append-only (SELECT,INSERT). No DELETE —
-- bookkeeping is append/update only. Guard on role existence like 0039/0040 so
-- a dev/test DB without the 0025 role model (superuser-only) still migrates.
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.creator_billing_status         TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT         ON zeroship.creator_billing_status_history TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.creator_billing_status FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.creator_billing_status_history FROM zeroship_control'; END IF; END $rb$;
