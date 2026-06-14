--liquibase formatted sql

-- DISPUTES / CHARGEBACKS (billing-ops gap #26, PR-8; design `0052 disputes` — the
-- FINAL billing-ops changeset). A cardholder disputes a charge; Stripe fires
-- `charge.dispute.created` and DEBITS the platform's balance immediately (the funds
-- are held by the network). This is NOT a refund we initiated — it is a FORCED
-- reversal — so it is its OWN fact, append-only, FK→the disputed invoice. The webhook
-- handler (a new branch in stripe_handlers, claim-after-success on `stripe_events_seen`
-- as usual) inserts/controlled-updates this row off the dispute lifecycle events
-- (created → updated → won/lost). The cash clawback is a SEPARATE negative
-- `invoice_payments` row (kind='dispute_debit'), so `Σ(invoice_payments)` (PR-3's
-- over-refund anchor) tightens automatically — no cross-table trigger.
--
-- CHANGELOG ORDER: this is the LAST billing-ops changeset (lexicographic 0053, after
-- 0052 notifications). It references `invoices(id)` (0042) only, which is far earlier,
-- so no ordering pin is needed. The `dispute_debit`/`dispute_reversal` payment rows it
-- drives go into `invoice_payments` (0046), also earlier.
--
-- RLS posture: creator-keyed / control-internal via the invoice's creator (NOT
-- app-keyed) ⇒ NO app FORCE RLS — uniform with invoices / invoice_payments / refunds
-- (control runs BYPASSRLS; the key is a creator's invoice, not a tenant app_id). Real
-- FK→invoices(id) ⇒ no orphan dispute.

--changeset zeroship:dispute-status-domain splitStatements:true
-- Stripe dispute status, membership only. The platform collapses Stripe's many
-- lifecycle statuses (warning_needs_response/needs_response/under_review/… ) into the
-- three terminal-relevant buckets the cash model cares about:
--   'open' = the dispute is live; the cash is network-held (a dispute_debit stands).
--   'won'  = funds returned to the platform (a dispute_reversal restores the budget).
--   'lost' = chargeback final, funds gone (the debit stays).
-- Authored BEFORE the table that references it (domains before tables, mirroring 0037).
CREATE DOMAIN zeroship.dispute_status AS TEXT
    CHECK (VALUE IN ('open','won','lost'));
--rollback DROP DOMAIN IF EXISTS zeroship.dispute_status;

--changeset zeroship:billing-disputes splitStatements:true
CREATE TABLE zeroship.billing_disputes (
    id          TEXT        PRIMARY KEY,                -- dsp_<base62> (disjoint 3-char prefix, MINOR-3)
    invoice_id  TEXT        NOT NULL REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    amount_cents BIGINT     NOT NULL CHECK (amount_cents > 0),  -- disputed amount (network-held)
    currency    CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    status      zeroship.dispute_status NOT NULL DEFAULT 'open',
    reason      TEXT,                                   -- Stripe dispute.reason (fraudulent/duplicate/…)
    -- Evidence-submission deadline from Stripe (dispute.evidence_details.due_by). NULL
    -- when Stripe omits it (a warning/inquiry with no formal due date).
    evidence_due_at TIMESTAMPTZ,
    -- The Stripe dispute id (du_…/dp_…). A real provider ref, kept INLINE because a
    -- dispute is ALWAYS provider-originated (no Native dispute concept) — unlike refunds,
    -- there is no provider-agnostic dispute we initiate, so a side table buys nothing.
    -- UNIQUE ⇒ ONE row per Stripe dispute: a redelivered created/updated/closed event
    -- for the SAME du_… is an idempotent UPSERT/controlled-update, never a second row.
    provider_dispute_id TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at TIMESTAMPTZ,
    UNIQUE (provider_dispute_id)
);
CREATE INDEX billing_disputes_invoice_idx ON zeroship.billing_disputes (invoice_id);
--rollback DROP INDEX IF EXISTS zeroship.billing_disputes_invoice_idx;
--rollback DROP TABLE zeroship.billing_disputes;

--changeset zeroship:invoice-payments-dispute-idempotency splitStatements:true
-- DISPUTE-ROW IDEMPOTENCY (PR-8, do-not-regress: "the dispute_debit/dispute_reversal
-- appends should be idempotent on the Stripe du_…/event id"). The charge-row dedup index
-- (0046) covers ONLY `kind='charge'`. The dispute clawback/reversal rows need their own
-- per-dispute uniqueness so a REDELIVERED `charge.dispute.created` (or `.closed won`)
-- under the same `du_…` does NOT append a SECOND negative debit (which would
-- over-tighten cash_collected) or a SECOND positive reversal (which would over-restore
-- it). The dedup key is `(invoice_id, provider_ref, kind)` over the dispute kinds:
--   * KIND is in the key so ONE dispute can carry BOTH a `dispute_debit` (on created)
--     AND a later `dispute_reversal` (on won) — they differ by kind, so both fit.
--   * a duplicate debit / duplicate reversal for the SAME du_… collides and the append's
--     `ON CONFLICT DO NOTHING` makes it a no-op (the `stripe_events_seen` gate is the
--     first line; this index is the durable backstop for a same-du_… redelivery under a
--     DIFFERENT event id, exactly as the charge index backstops invoice.paid redelivery).
-- `provider_ref` for dispute rows is the `du_…` (always supplied), so the partial index
-- is fully covering.
CREATE UNIQUE INDEX invoice_payments_dispute_provider_ref_key
    ON zeroship.invoice_payments (invoice_id, provider_ref, kind)
    WHERE kind IN ('dispute_debit','dispute_reversal');
--rollback DROP INDEX IF EXISTS zeroship.invoice_payments_dispute_provider_ref_key;

--changeset zeroship:billing-disputes-controlled-immutable splitStatements:false
-- A dispute row is append-OR-controlled-update only: the financial facts are FROZEN at
-- insert (invoice_id / amount_cents / currency / provider_dispute_id NEVER change), and
-- the ONLY legal mutation is the lifecycle progression of `status` (open → won/lost) plus
-- its `resolved_at`/`reason`/`evidence_due_at` metadata, driven by later
-- charge.dispute.updated/closed events for the SAME du_…. DELETE is rejected outright
-- (uniform with invoice_payments / credit_ledger immutability). This blocks a stray
-- writer from retargeting a dispute at a different invoice or rewriting the clawed-back
-- amount after the fact, while still allowing the won/lost resolution the webhook needs.
CREATE FUNCTION zeroship.billing_disputes_controlled_update() RETURNS trigger AS $fn$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'billing_disputes is append-only (no DELETE) — a dispute is a permanent fact';
    END IF;
    -- The frozen financial identity columns may never change.
    IF NEW.id <> OLD.id
       OR NEW.invoice_id <> OLD.invoice_id
       OR NEW.amount_cents <> OLD.amount_cents
       OR NEW.currency <> OLD.currency
       OR NEW.provider_dispute_id <> OLD.provider_dispute_id
       OR NEW.created_at <> OLD.created_at THEN
        RAISE EXCEPTION 'billing_disputes frozen columns are immutable (invoice_id/amount_cents/currency/provider_dispute_id) — only status/resolved_at/reason/evidence_due_at may progress';
    END IF;
    -- STATUS LIFECYCLE (PR-8 CRITICAL-3 / MAJOR-7): the status is a STRICT one-way
    -- progression `open → won|lost`. A terminal row NEVER moves again — so a
    -- redelivered/out-of-order/late terminal event (e.g. a `won` followed by a stale
    -- `lost`) can NOT flip an already-resolved dispute and strand restored cash on a
    -- now-`lost` dispute (an over-refund window). The application layer gates the close
    -- UPDATE with `WHERE status='open'` so this is normally a no-op; this trigger is the
    -- DURABLE backstop that rejects any `won→lost` / `lost→won` / `*→open` / terminal→*
    -- transition outright. A no-op same-status UPDATE (OLD.status = NEW.status) is allowed
    -- (idempotent metadata touch).
    IF NEW.status <> OLD.status THEN
        IF NOT (OLD.status = 'open' AND NEW.status IN ('won','lost')) THEN
            RAISE EXCEPTION 'billing_disputes status may only progress open→won/lost (got %→%) — a terminal dispute is frozen',
                OLD.status, NEW.status;
        END IF;
    END IF;
    RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER billing_disputes_controlled_update_trg
    BEFORE UPDATE OR DELETE ON zeroship.billing_disputes
    FOR EACH ROW EXECUTE FUNCTION zeroship.billing_disputes_controlled_update();
--rollback DROP TRIGGER IF EXISTS billing_disputes_controlled_update_trg ON zeroship.billing_disputes;
--rollback DROP FUNCTION IF EXISTS zeroship.billing_disputes_controlled_update();

--changeset zeroship:billing-disputes-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- SELECT/INSERT/UPDATE: UPDATE is needed for the open→won/lost lifecycle progression
    -- (the controlled-update trigger above bounds WHAT may change). NO DELETE
    -- (append-only; a dispute is a permanent fact).
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_disputes TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_disputes FROM zeroship_control'; END IF; END $rb$;
