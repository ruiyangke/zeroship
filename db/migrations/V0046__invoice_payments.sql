-- PAYMENTS (billing-ops gap #26, PR-1; design round 2 CRITICAL-A). The cash-collected
-- anchor for the over-refund cap (PR-3) lives here, NOT as a column on `invoices`.
-- Round 1 tried to freeze cash-collected as a column written by the payment webhook —
-- but `invoices_immutable()` (0042) RAISEs on EVERY finalized→finalized UPDATE (only
-- →void with money held equal is legal), so that write is mechanically IMPOSSIBLE. This
-- is the design's own core principle applied correctly: a payment is an APPEND-ONLY SIDE
-- FACT against a finalized invoice, never a mutation of it. One row per payment /
-- partial-pay / dispute-clawback. cash_collected(invoice) = Σ(amount_cents) over these
-- rows. The over-refund trigger (PR-3 / 0049), the dispute check, the true-up bridge and
-- the read API all read THIS sum. Because the invoice row is never touched, the
-- immutability trigger is never even challenged.
--
-- CHANGELOG ORDER (design ordering note): this table MUST exist before 0049's over-refund
-- trigger function references it. The master changelog uses Liquibase `includeAll` with
-- LEXICOGRAPHIC filename order, so the file is numbered 0046 (< 0048 credits, < 0049
-- refunds) to guarantee it lands first. (The design narrative calls it "0053" — that
-- number predates the realisation that this repo's master is includeAll, not an explicit
-- list; lexicographic order is what actually pins "before 0049", so a sub-0049 number is
-- the faithful realisation of the design's intent. See the PR-1 report.)
--
-- RLS posture: creator-keyed / control-internal via the invoice's creator (NOT app-keyed)
-- ⇒ NO app FORCE RLS — uniform with invoices / billing_provider_refs (control runs
-- BYPASSRLS; the key is a creator's invoice, not a tenant app_id). Real FK→invoices(id)
-- ⇒ no orphan payment.

-- Provenance kinds (membership only). 'charge' = invoice.paid / charge.succeeded (full or
-- partial, POSITIVE); 'dispute_debit' = charge.dispute.created clawback (NEGATIVE);
-- 'dispute_reversal' = dispute won, funds back (POSITIVE). Authored BEFORE the table that
-- references it (domains before tables, mirroring 0037).
CREATE DOMAIN zeroship.invoice_payment_kind AS TEXT
    CHECK (VALUE IN ('charge','dispute_debit','dispute_reversal'));

CREATE TABLE zeroship.invoice_payments (
    id          TEXT        PRIMARY KEY,                -- ipy_<base62> (disjoint 3-char prefix)
    invoice_id  TEXT        NOT NULL REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    -- SIGNED: a normal payment / partial-pay is POSITIVE (cash came in). A dispute that
    -- claws cash back is NEGATIVE (cash left), so Σ(amount_cents) is the NET cash the
    -- platform currently holds for this invoice — exactly the right over-refund anchor:
    -- a lost dispute lowers refundable cash automatically, no cross-table trigger. (A
    -- 'won' dispute that returns funds appends a compensating POSITIVE row.) A zero
    -- payment is meaningless (CHECK <> 0). A FULLY-credited invoice (total_cents = 0)
    -- writes NO row — there was no charge — so cash-collected stays 0, exactly right.
    amount_cents BIGINT     NOT NULL CHECK (amount_cents <> 0),
    currency    CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    kind        zeroship.invoice_payment_kind NOT NULL,
    -- The provider event that produced this row (pi_…/ch_…/in_…/dp_…), for audit + dedup.
    provider_ref TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX invoice_payments_invoice_idx ON zeroship.invoice_payments (invoice_id);
-- IDEMPOTENCY KEY (billing-ops gap #26 review, CRITICAL-1). The webhook's
-- `record_infra_payment` appends a `charge` row from `invoice.paid`. The outer
-- `stripe_events_seen` event-id gate is NOT sufficient to make the APPEND
-- idempotent: (a) the append runs before the fallible payout path, so a payout
-- error → non-2xx → event NOT claimed → Stripe retries the SAME `evt_id` while a
-- charge row is already committed from the first pass; (b) Stripe can redeliver
-- `invoice.paid` under a NEW `evt_id` (re-finalize / uncollectible-then-paid)
-- carrying the cumulative `amount_paid`. Both append a second row → cash_collected
-- over-counts → PR-3's over-refund cap inflates. The real dedup key is the Stripe
-- payment object (`in_…`/`pi_…`) carried in `provider_ref`. A PARTIAL unique index
-- on `kind='charge'` rows (which ALWAYS supply `provider_ref`) lets `append_charge`
-- do `INSERT … ON CONFLICT DO NOTHING`, so any number of retries/redeliveries for
-- the same Stripe payment append EXACTLY ONE row. `provider_ref` stays nullable
-- (dispute_debit/dispute_reversal rows may omit it) — the index covers only the
-- charge rows, which never do.
CREATE UNIQUE INDEX invoice_payments_charge_provider_ref_key
    ON zeroship.invoice_payments (invoice_id, provider_ref) WHERE kind = 'charge';

-- A payment receipt is append-only (uniform with credit_ledger / invoice immutability):
-- a correction is a NEW offsetting row (a negative dispute_debit), never an edit. Reject
-- UPDATE and DELETE outright.
CREATE FUNCTION zeroship.invoice_payments_immutable() RETURNS trigger AS $fn$
BEGIN
    RAISE EXCEPTION 'invoice_payments is append-only (no UPDATE/DELETE) — correct via a new offsetting row';
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER invoice_payments_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.invoice_payments
    FOR EACH ROW EXECUTE FUNCTION zeroship.invoice_payments_immutable();

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- INSERT-only beyond SELECT: append-only (the trigger is the backstop; the grant is
    -- the first line of defence — least privilege). NO UPDATE/DELETE.
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.invoice_payments TO zeroship_control';
  END IF;
END $g$;
