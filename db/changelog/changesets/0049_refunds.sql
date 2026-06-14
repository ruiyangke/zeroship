--liquibase formatted sql

-- REFUNDS (billing-ops gap #26, PR-3; design `0049 refunds` + flow B "refund a
-- finalized invoice"). A refund is a SEPARATE append-only fact with a REAL FK to the
-- PAID invoice — NEVER a mutation of the finalized invoice (the immutability trigger
-- would reject it anyway). The invoice stays status='finalized'. For destination='cash'
-- the money-movement object is a Stripe REFUND (re_…) on the original
-- charge/PaymentIntent (a credit note cn_… is an OPTIONAL bookkeeping wrapper, NOT the
-- refund — verified against docs.stripe.com/api/refunds/create: a Refund "Funds will be
-- refunded to the credit or debit card that was originally charged"; a credit note alone
-- does not move cash on a paid invoice). For destination='credit' the refund is a
-- platform-native credit_ledger('refund_to_credit') grant — no charge reversal.
--
-- CHANGELOG ORDER: 0049's over-refund trigger references `invoice_payments` (0046,
-- landed in PR-1) — lexicographic includeAll guarantees 0046 < 0048 < 0049, so the
-- table the trigger reads already exists. (The design narrative calls invoice_payments
-- "0053"; this repo's master is includeAll/lexicographic, so a sub-0049 number is the
-- faithful realisation — see the 0046 header + the PR-1 report.)
--
-- RLS posture: creator-keyed / control-internal via the invoice's creator (NOT
-- app-keyed) ⇒ NO app FORCE RLS — uniform with invoices / invoice_payments / credit_ledger
-- (control runs BYPASSRLS; the key is a creator's invoice, not a tenant app_id). Real
-- FK→invoices(id) ⇒ no orphan refund.

--changeset zeroship:refund-domains splitStatements:true
-- Destination (DECISION 3): 'cash' = a Stripe Refund (re_…) back to the card; 'credit'
-- = a credit_ledger 'refund_to_credit' grant instead of cash. Lifecycle (DECISION,
-- MINOR-2): 'pending' the instant the row is claimed (intent), 'issued' after the
-- provider call succeeds and its ref is written. 'void' is DROPPED — you cannot un-refund
-- cash; a mistaken refund is corrected by a fresh re-charge / negative-invoice true-up,
-- not by un-refunding (see the void+reissue true-up bridge in void_reissue.rs). Membership
-- only. Authored BEFORE the table that references them (domains before tables, 0037/0046/0048).
CREATE DOMAIN zeroship.refund_destination AS TEXT
    CHECK (VALUE IN ('cash','credit'));
CREATE DOMAIN zeroship.refund_status AS TEXT
    CHECK (VALUE IN ('pending','issued'));
--rollback DROP DOMAIN IF EXISTS zeroship.refund_status;
--rollback DROP DOMAIN IF EXISTS zeroship.refund_destination;

--changeset zeroship:refunds splitStatements:true
CREATE TABLE zeroship.refunds (
    id          TEXT        PRIMARY KEY,                -- ref_<base62> (disjoint 3-char prefix)
    invoice_id  TEXT        NOT NULL REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    -- TAX-ON-REFUND (MISSING-6): a refund of a TAXED invoice must return proportional
    -- tax. amount_cents is split into the pre-tax portion and the tax portion so the
    -- Stripe Refund / credit-note line carries the right tax split and the platform's
    -- tax-liability books stay correct. amount_cents = subtotal + tax (CHECK). For a
    -- zero-tax (USD-launch) invoice tax_cents = 0 and the split is degenerate, but the
    -- column exists so enabling Stripe Tax later needs no reshape.
    amount_cents   BIGINT  NOT NULL CHECK (amount_cents > 0),
    subtotal_cents BIGINT  NOT NULL CHECK (subtotal_cents >= 0),
    tax_cents      BIGINT  NOT NULL DEFAULT 0 CHECK (tax_cents >= 0),
    CONSTRAINT refund_amount_split CHECK (amount_cents = subtotal_cents + tax_cents),
    currency    CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    destination zeroship.refund_destination NOT NULL,
    reason      TEXT,                                   -- operator audit; PII posture: see redaction note
    -- IDEMPOTENCY (MISSING-7): the operator-supplied idempotency key. A
    -- double-clicked / retried POST /invoices/{id}/refunds carries the SAME key, so the
    -- UNIQUE(idempotency_key) below makes the second INSERT a no-op (the first refund is
    -- returned) — NO double refund. NOT NULL: the endpoint requires it.
    idempotency_key TEXT NOT NULL,
    -- REQUEST-BODY FINGERPRINT (MINOR-A): a globally-UNIQUE key with NO body check
    -- silently returns the FIRST refund even if a caller REUSES the key with a DIFFERENT
    -- amount/destination — masking a real bug. Stripe 400s on key-reuse-with-different-body
    -- (verified at docs.stripe.com/api/idempotent_requests). We mirror that: the endpoint
    -- stores a SHA-256 over the canonical refund request (invoice_id, amount_cents,
    -- subtotal_cents, tax_cents, destination) here; on a key hit, if the stored fingerprint
    -- != the new request's fingerprint the endpoint returns 409, NOT the first refund.
    -- Same key + same body ⇒ return the first refund (safe retry).
    request_fingerprint TEXT NOT NULL,
    status      zeroship.refund_status NOT NULL DEFAULT 'pending',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    issued_at   TIMESTAMPTZ,
    UNIQUE (idempotency_key)
);
CREATE INDEX refunds_invoice_idx ON zeroship.refunds (invoice_id);
--rollback DROP INDEX IF EXISTS zeroship.refunds_invoice_idx;
--rollback DROP TABLE zeroship.refunds;

--changeset zeroship:refund-provider-refs splitStatements:true
-- The provider seam for refunds. CRITICAL-5: ref_kind allows BOTH 'refund' (re_… —
-- the cash money-movement object, the authoritative ref for destination='cash') AND
-- 'credit_note' (cn_… — the OPTIONAL bookkeeping wrapper) AND 'customer_balance_txn'
-- (cbtxn_… — the Stripe-side credit path, if used). Mirrors billing_line_provider_refs:
-- written CLAIM-AFTER-SUCCESS so a refund whose POST errored has NO ref and the re-drive
-- re-issues it (idempotent on the deterministic provider idempotency key); a refund WITH a
-- ref is skipped. A malformed key cannot be inserted (the FK rejects it), so the
-- double-refund guard cannot silently fail-to-match.
CREATE TABLE zeroship.refund_provider_refs (
    refund_id   TEXT NOT NULL REFERENCES zeroship.refunds(id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,                -- 'stripe'
    ref_kind    TEXT NOT NULL,                -- 'refund' (re_…) | 'credit_note' (cn_…) | 'customer_balance_txn'
    external_id TEXT NOT NULL,                -- re_… / cn_… / cbtxn_…
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (refund_id, provider, ref_kind),
    UNIQUE (provider, ref_kind, external_id)
);
--rollback DROP TABLE zeroship.refund_provider_refs;

--changeset zeroship:refund-over-refund-guard splitStatements:false
-- OVER-REFUND BACKSTOP (CRITICAL-1; CRITICAL-A: read Σ(invoice_payments), NOT a
-- cash_collected_cents column). The cap is anchored on the cash ACTUALLY collected —
-- the SUM of the append-only invoice_payments rows for this invoice — NOT total_cents
-- (which is credit-inflated; capping on it enables credit laundering). THREE bounds
-- enforced simultaneously:
--   (i)  Σ(cash refunds)               ≤ cash_collected
--   (ii) Σ(credit-destination refunds) ≤ cash_collected
--   (iii) Σ(all refunds, any dest)     ≤ cash_collected
-- (i)+(ii) stop EITHER channel alone exceeding the cash actually collected; (iii) stops
-- the two channels SUMMING past it. Credit-funded value (total_cents − cash_collected) is
-- NEVER re-granted as cash OR as fresh credit — the credit-laundering defence. A CHECK
-- cannot span rows, so this is a BEFORE-INSERT trigger (the DB-level backstop; the Rust
-- path also checks before claiming). status carries no 'void', so every existing refund
-- row counts. invoice_payments (0046) exists before this function is created (lexicographic
-- changelog order); the SELECT references it directly so 0049 needs no helper function.
CREATE FUNCTION zeroship.refunds_no_over_refund() RETURNS trigger AS $fn$
DECLARE cash BIGINT; sum_cash BIGINT; sum_credit BIGINT;
BEGIN
    -- cash collected = Σ(invoice_payments), not a frozen column.
    SELECT COALESCE(SUM(amount_cents), 0) INTO cash
      FROM zeroship.invoice_payments WHERE invoice_id = NEW.invoice_id;
    SELECT COALESCE(SUM(amount_cents) FILTER (WHERE destination = 'cash'),   0),
           COALESCE(SUM(amount_cents) FILTER (WHERE destination = 'credit'), 0)
      INTO sum_cash, sum_credit
      FROM zeroship.refunds
      WHERE invoice_id = NEW.invoice_id AND id <> NEW.id;
    IF NEW.destination = 'cash'   THEN sum_cash   := sum_cash   + NEW.amount_cents; END IF;
    IF NEW.destination = 'credit' THEN sum_credit := sum_credit + NEW.amount_cents; END IF;
    IF sum_cash > cash THEN
        RAISE EXCEPTION 'refund % over-refunds CASH on invoice % (cash refunds % > Σ(invoice_payments) %)',
            NEW.id, NEW.invoice_id, sum_cash, cash;
    END IF;
    IF sum_credit > cash THEN
        RAISE EXCEPTION 'refund % over-refunds CREDIT-DEST on invoice % (credit refunds % > Σ(invoice_payments) %)',
            NEW.id, NEW.invoice_id, sum_credit, cash;
    END IF;
    IF sum_cash + sum_credit > cash THEN
        RAISE EXCEPTION 'refund % over-refunds COMBINED on invoice % (% > Σ(invoice_payments) %)',
            NEW.id, NEW.invoice_id, sum_cash + sum_credit, cash;
    END IF;
    RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;
-- INSERT-only: a refund row's amount/destination never change after claim (no 'void'
-- to flip), so re-checking on UPDATE is unnecessary; the pending→issued status flip does
-- not touch money columns.
CREATE TRIGGER refunds_no_over_refund_trg
    BEFORE INSERT ON zeroship.refunds
    FOR EACH ROW EXECUTE FUNCTION zeroship.refunds_no_over_refund();
--rollback DROP TRIGGER IF EXISTS refunds_no_over_refund_trg ON zeroship.refunds;
--rollback DROP FUNCTION IF EXISTS zeroship.refunds_no_over_refund();

--changeset zeroship:refunds-immutable splitStatements:false
-- A refund row's MONEY is append-only (uniform with invoice/payment/credit immutability):
-- only the pending→issued status flip (+ issued_at) is a legal UPDATE; the amount split,
-- destination, invoice_id, currency, and idempotency identity are FROZEN at claim. A
-- mistaken refund is corrected by a true-up / re-charge, never by editing the row or
-- DELETEing it.
CREATE FUNCTION zeroship.refunds_immutable() RETURNS trigger AS $fn$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'refunds are append-only (no DELETE) — correct via a true-up / re-charge';
    END IF;
    IF NEW.id <> OLD.id
       OR NEW.invoice_id <> OLD.invoice_id
       OR NEW.amount_cents <> OLD.amount_cents
       OR NEW.subtotal_cents <> OLD.subtotal_cents
       OR NEW.tax_cents <> OLD.tax_cents
       OR NEW.currency <> OLD.currency
       OR NEW.destination <> OLD.destination
       OR NEW.idempotency_key <> OLD.idempotency_key
       OR NEW.request_fingerprint <> OLD.request_fingerprint THEN
        RAISE EXCEPTION 'refund % is frozen — only the pending→issued status flip is permitted', OLD.id;
    END IF;
    RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER refunds_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.refunds
    FOR EACH ROW EXECUTE FUNCTION zeroship.refunds_immutable();
--rollback DROP TRIGGER IF EXISTS refunds_immutable_trg ON zeroship.refunds;
--rollback DROP FUNCTION IF EXISTS zeroship.refunds_immutable();

--changeset zeroship:refunds-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- refunds: SELECT/INSERT/UPDATE (pending→issued status flip + issued_at). No DELETE
    -- (append-only; a mistaken refund is corrected by a true-up, not deleted).
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.refunds              TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT         ON zeroship.refund_provider_refs TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.refunds FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.refund_provider_refs FROM zeroship_control'; END IF; END $rb$;
