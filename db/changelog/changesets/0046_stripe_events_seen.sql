--liquibase formatted sql

-- ════════════════════════════════════════════════════════════════════════
-- Stripe webhook replay-dedup ledger (billing G6): process each verified
-- webhook event AT MOST ONCE.
-- ════════════════════════════════════════════════════════════════════════
--
-- Stripe delivers webhooks AT-LEAST-ONCE: a validly-signed event is routinely
-- re-delivered (timeout, retry storm, manual replay). Before this ledger only
-- the payout path (`payouts.event_id UNIQUE`) deduped; the account-state
-- handlers (`setup_intent.succeeded`, `invoice.payment_failed`, the
-- `invoice.paid` recovery) had only their own state guards and would re-run
-- their side effects + emit a duplicate audit row on every redelivery.
--
-- This is the GENERAL ledger: one row per webhook event-id, written by the
-- webhook handler AFTER its handler succeeds (claim-AFTER-success), so a
-- handler that errored is NOT recorded and Stripe's retry re-processes it
-- (exactly-once EFFECTIVE: no double-process, no lost event on failure). The
-- narrower `payouts.event_id` dedup stays as a defense-in-depth backstop for
-- the revenue ledger; this ledger generalizes dedup to ALL event types at the
-- top of the dispatcher.
--
-- Mirrors `usage_reports_seen` (changeset 0037): append-only, control-internal
-- bookkeeping, NO RLS (not app-keyed — keyed by Stripe event-id), least-priv
-- grants with NO DELETE.
--
-- Pre-launch, no back-compat: additive DDL, no backfill (no production data).

-- ─── stripe_events_seen (idempotent dedup ledger) ──────────────────────────
--changeset zeroship:stripe-events-seen splitStatements:true
CREATE TABLE zeroship.stripe_events_seen (
    -- Stripe's `evt_…` event id. PRIMARY KEY ⇒ the dedup key: an
    -- INSERT … ON CONFLICT DO NOTHING affecting 0 rows means "already
    -- processed" → the webhook acks 200 without re-dispatching.
    event_id   TEXT        PRIMARY KEY,
    event_type TEXT        NOT NULL,
    seen_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.stripe_events_seen;

-- ─── Grants (control is the only writer; mirrors usage_reports_seen) ────────
-- The webhook handler claims (INSERT) + checks (SELECT) processed events. No
-- DELETE/UPDATE: this is an append-only dedup ledger. Guard on role existence
-- like 0037/0045 so a dev/test DB without the 0025 role model still migrates.
--changeset zeroship:stripe-events-seen-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.stripe_events_seen TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.stripe_events_seen FROM zeroship_control'; END IF; END $rb$;
