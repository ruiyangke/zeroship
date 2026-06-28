-- Account/dunning state. The gateway route-pull LEFT JOINs this onto each app via
-- app_members(owner) to populate RouteEntry.account_state; account_status.rs UPSERTs the
-- payment-failure→past_due→suspension lifecycle; dunning.rs scans it.
-- creator_id FK targets creator_billing(creator_id), NOT users directly, so the FK +
-- erase policy is UNIFORM across the billing cluster. ON DELETE CASCADE — transitively
-- through creator_billing → users — so an anonymize-retained creator keeps this row (FK
-- target alive); a hard-deleted (never-billed) creator's row CASCADEs away cleanly.
-- Creator-keyed ⇒ NO per-app RLS; the gateway projects it per-app via app_members(owner).
CREATE TABLE zeroship.creator_billing_status (
    creator_id              UUID PRIMARY KEY REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    state                   zeroship.account_state NOT NULL DEFAULT 'active',
    past_due_since          TIMESTAMPTZ,                -- set on active→past_due; cleared on recovery
    suspended_at            TIMESTAMPTZ,                -- NULL unless suspended
    last_payment_failure_at TIMESTAMPTZ,                -- audit: most recent failed-payment signal
    failed_invoice_id       TEXT,                       -- audit: WHICH invoice last failed
    -- G2 ORDER-SAFETY (account_status.rs critic #1) — CARRIED FORWARD VERBATIM
    -- from the live 0045. Stripe webhooks reorder/redeliver, so a stale
    -- `payment_failed` can land AFTER an `invoice.paid` recovery. These two
    -- high-water columns are the false-suspend guard and are READ + WRITTEN by
    -- account_status.rs (record_payment_failed / record_payment_recovered):
    --   * last_recovered_at — the Stripe `event.created` of the most recent
    --     RECOVERY (advanced monotonically via GREATEST). A `payment_failed`
    --     whose `event.created <= last_recovered_at` is STALE and is IGNORED —
    --     it MUST NOT re-arm past_due on an already-paying creator.
    --   * last_event_at — the `event.created` of the most recent event applied
    --     (monotonic audit bookkeeping).
    -- DROPPING EITHER COLUMN regresses the G2 order-safe suspension (the live
    -- code would fail with "column does not exist"). Both default NULL.
    last_event_at           TIMESTAMPTZ,
    last_recovered_at       TIMESTAMPTZ,
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- `id` (billing-ops gap #26, PR-6 notifications) is a STABLE surrogate PK = the
-- `billing_notifications.transition_id` for the dunning-driven notification kinds
-- (payment_failed/past_due/suspended/recovered). A `cbh_<base62>` typed id minted in
-- Rust by `account_status.rs::append_history` (NO SQL DEFAULT — see the `she_` note in
-- 0041; the prefix must stay disjoint from `she`/`inv`/`ref`/`dsp`, design MINOR-3).
-- Added in PR-6 (pre-launch, edited in place — clean re-migrate, no live ALTER).
CREATE TABLE zeroship.creator_billing_status_history (
    id         TEXT NOT NULL PRIMARY KEY,  -- cbh_<base62> (PR-6)
    creator_id UUID NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    from_state zeroship.account_state NOT NULL,
    to_state   zeroship.account_state NOT NULL,
    reason     TEXT,                      -- 'payment_failed' | 'dunning_exhausted' | 'payment_recovered'
    at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_creator_billing_status_history_creator_at
    ON zeroship.creator_billing_status_history (creator_id, at DESC);
-- Partial index: the dunning cron scans past_due rows by past_due_since.
CREATE INDEX idx_creator_billing_status_past_due
    ON zeroship.creator_billing_status (past_due_since)
    WHERE state = 'past_due';

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.creator_billing_status         TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT         ON zeroship.creator_billing_status_history TO zeroship_control';
  END IF;
END $g$;

-- G6 WEBHOOK REPLAY-DEDUP — CARRIED FORWARD VERBATIM from the live 0046.
-- Stripe delivers webhooks AT-LEAST-ONCE; this is the GENERAL dedup ledger so
-- each verified event processes AT-MOST-ONCE. Written CLAIM-AFTER-SUCCESS by the
-- webhook dispatcher (stripe_handlers.rs:582 event_processed / :603
-- mark_event_processed → stripe_store.rs:453/472), so a handler that errored is
-- NOT recorded and Stripe's retry re-processes it (exactly-once EFFECTIVE).
-- Mirrors usage_reports_seen: control-internal, NOT app-keyed (keyed by the
-- Stripe evt_… id) ⇒ NO RLS; append-only ⇒ no DELETE/UPDATE grant.
CREATE TABLE zeroship.stripe_events_seen (
    event_id   TEXT        PRIMARY KEY,       -- evt_… ; the dedup key
    event_type TEXT        NOT NULL,
    seen_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.stripe_events_seen TO zeroship_control';
  END IF;
END $g$;
