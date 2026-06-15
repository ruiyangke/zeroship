--liquibase formatted sql

-- PENDING (UNLINKED) DISPUTES — order-independence for `charge.dispute.created`
-- vs. `invoice.paid` (billing-ops gap #26 follow-up; the dispute-vs-linkage race the
-- live Stripe-delivered e2e surfaced).
--
-- THE HOLE THIS CLOSES. A `charge.dispute.*` object carries NO `invoice` field — only
-- `payment_intent` (pi_…) and `charge` (ch_…). The dispute handler resolves THOSE back
-- to our internal invoice via the `billing_provider_refs(ref_kind IN
-- ('payment_intent','charge'))` linkage that `invoice.paid` records at payment time
-- (0042 + record_payment_object_refs). But Stripe gives only at-least-once, UNORDERED
-- delivery: a `charge.dispute.created` can arrive BEFORE the `invoice.paid` that writes
-- that linkage. Pre-fix the handler found no linkage, acked 200 ("no_internal_invoice")
-- and DROPPED the dispute — and it was NOT self-healing: the later `invoice.paid` wrote
-- the linkage but never re-checked for the dropped dispute, and a Stripe resend is
-- dedup-acked by `stripe_events_seen`. Net: the dispute_debit was never applied and the
-- over-refund cap never tightened → a creator could refund cash that was charged back.
--
-- THE FIX (order-independent, mirroring the close-before-create MAJOR-4 handling). When
-- `charge.dispute.created` cannot resolve a linkage YET, we PARK the dispute facts here
-- (idempotent on the du_…) instead of dropping them. When the settling pi_…/ch_…→invoice
-- linkage is FIRST written (record_payment_object_refs, driven by invoice.paid), we look
-- for any parked dispute matching that pi_/ch_ and RESOLVE it in the SAME txn: promote it
-- to a `billing_disputes` row + apply the `dispute_debit` (tightening the cap) + delete
-- the holding row. End state is IDENTICAL regardless of delivery order, exactly-once on
-- the du_… (the billing_disputes UNIQUE(provider_dispute_id) + the dispute payment-row
-- dedup index 0053 make the promotion idempotent). A genuinely-unrecognized charge's
-- dispute (a Connect end-user charge the platform never invoiced) parks harmlessly and
-- never resolves — no poison, no 5xx-retry storm.
--
-- WHY A HOLDING TABLE (not a nullable billing_disputes.invoice_id). billing_disputes
-- (0053) has invoice_id NOT NULL REFERENCES invoices(id) plus a controlled-update trigger
-- that FREEZES invoice_id. A pending dispute has, by definition, NO resolved invoice yet —
-- relaxing that NOT NULL + the freeze would weaken the strongest invariant on the dispute
-- fact (it can never be retargeted). A small disjoint holding table keeps billing_disputes'
-- frozen-column guarantee intact: a row only ever LANDS in billing_disputes already linked.
--
-- CHANGELOG ORDER: after 0054 (lexicographic, includeAll). References nothing earlier by
-- FK (deliberately FK-free — the whole point is "not yet linked to an invoice"); the
-- promotion target billing_disputes (0053) is far earlier.
--
-- RLS posture: control-internal, no app_id (uniform with billing_disputes /
-- invoice_payments). Control runs BYPASSRLS; the holding row is provider-keyed only.

--changeset zeroship:pending-disputes splitStatements:true
CREATE TABLE zeroship.pending_disputes (
    -- The Stripe dispute id (du_…/dp_…) is the natural key: ONE parked row per dispute,
    -- so a redelivered created (under a fresh evt_… the stripe_events_seen gate misses)
    -- is an idempotent ON CONFLICT DO NOTHING, never a second parked row.
    provider_dispute_id TEXT PRIMARY KEY,
    -- The settling objects the dispute named — resolution candidates matched against the
    -- billing_provider_refs linkage once invoice.paid writes it. At least one is non-null
    -- (the handler parks only when a candidate existed but no linkage did yet).
    payment_intent TEXT,                            -- pi_…
    charge         TEXT,                            -- ch_…
    -- The disputed (clawed-back) amount + metadata, carried so the later promotion can
    -- build the billing_disputes row + its dispute_debit without re-fetching Stripe.
    amount_cents BIGINT     NOT NULL CHECK (amount_cents > 0),
    currency    CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    reason      TEXT,
    evidence_due_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- At least one resolution candidate must be present, else the row could never resolve.
    CHECK (payment_intent IS NOT NULL OR charge IS NOT NULL)
);
-- Resolution lookups hit by pi_… and by ch_… (the two candidate columns invoice.paid's
-- linkage can match). Partial indexes skip the NULL side.
CREATE INDEX pending_disputes_payment_intent_idx
    ON zeroship.pending_disputes (payment_intent) WHERE payment_intent IS NOT NULL;
CREATE INDEX pending_disputes_charge_idx
    ON zeroship.pending_disputes (charge) WHERE charge IS NOT NULL;
--rollback DROP INDEX IF EXISTS zeroship.pending_disputes_charge_idx;
--rollback DROP INDEX IF EXISTS zeroship.pending_disputes_payment_intent_idx;
--rollback DROP TABLE zeroship.pending_disputes;

--changeset zeroship:pending-disputes-grants splitStatements:false
-- SELECT/INSERT (park on dispute.created) + DELETE (drop on promotion at invoice.paid
-- linkage time). No UPDATE: a parked row is never mutated, only created then consumed.
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, DELETE ON zeroship.pending_disputes TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.pending_disputes FROM zeroship_control'; END IF; END $rb$;
