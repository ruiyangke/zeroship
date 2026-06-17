-- WEBHOOK EVENT FOLLOW-UPS (billing-ops gap #26 follow-up: the 3 deferred Stripe
-- webhook handlers that were loudly-logged-but-not-acted-on in `dispatch_event`).
-- This changeset adds the durable schema the three handlers need:
--
--   1. `charge.refund.updated` — a refund we recorded `issued` that LATER FAILS
--      (the bank rejected the credit) or is CANCELED. The cash did NOT return to the
--      cardholder, so our ledger must stop asserting a refund happened:
--        * the `refund_status` domain gains 'failed'/'canceled' (reconciled against the
--          Stripe Refund status enum: pending/requires_action/succeeded/failed/canceled
--          — verified docs.stripe.com/api/refunds/object). 'pending'/'issued' map to our
--          existing lifecycle; a Stripe 'failed'/'canceled' maps to ours 1:1.
--        * the refunds-immutable trigger is LOOSENED to permit the terminal failure
--          transitions (issued|pending → failed|canceled) while keeping the MONEY columns
--          frozen — so a failed refund can be marked without un-freezing the amount split.
--        * the over-refund cap (Rust precheck + the `refunds_no_over_refund` trigger) now
--          EXCLUDES failed/canceled refunds, so a failed cash refund no longer permanently
--          reduces refundable cash (the creator CAN re-refund).
--        * a failed `destination='credit'` refund's minted `refund_to_credit` grant is
--          CLAWED BACK by a compensating negative `credit_ledger('refund_clawback')` entry
--          (the cash never left Stripe, so the creator must not keep the credit). The
--          balance is conserved.
--
--   2. `payout.failed` — a payout to a creator's connected account bounced (bad bank
--      details / closed account). A new `payout_failures` append-only ledger records it,
--      idempotent on the Stripe payout id (`po_…`); the notify cron emits a
--      `payout_failed` creator notification off the new row.
--
--   3. `payment_intent.payment_failed` — an end-user's Connect checkout charge failed.
--      No money moved (informational), but it must be SURFACED not silently dropped. A new
--      `connect_checkout_failures` append-only ledger records it, idempotent on the
--      PaymentIntent id (`pi_…`); the notify cron emits a `checkout_failed` notification.
--
-- CHANGELOG ORDER: lexicographic includeAll. This is 0054 — after 0048 (credit_ledger),
-- 0049 (refunds), 0052 (billing_notifications) and 0053 (billing_disputes), all of which
-- it references / extends, so the targets exist when this runs.
--
-- RLS posture: creator-keyed control bookkeeping ⇒ NO app FORCE RLS (control runs
-- BYPASSRLS; the key is a creator's connected account / invoice, not a tenant app_id) —
-- uniform with refunds / disputes / billing_notifications. Real FKs ⇒ no orphans.

-- ─────────────────────────────────────────────────────────────────────────────
-- (1) charge.refund.updated — failed/canceled refund reconciliation
-- ─────────────────────────────────────────────────────────────────────────────

-- Extend the refund lifecycle with the two TERMINAL FAILURE states a `charge.refund.updated`
-- can drive a previously-`issued` refund into. A Postgres DOMAIN CHECK cannot be ALTERed in
-- place, so DROP + recreate the CHECK constraint via ALTER DOMAIN (pre-launch, clean
-- re-migrate). The membership widens to {pending, issued, failed, canceled}.
ALTER DOMAIN zeroship.refund_status DROP CONSTRAINT refund_status_check;
ALTER DOMAIN zeroship.refund_status ADD CONSTRAINT refund_status_check
    CHECK (VALUE IN ('pending','issued','failed','canceled'));

-- When a refund went terminal-failed/canceled (the cash bounced). NULL for a healthy
-- pending/issued refund. Audit + the idempotency anchor for `charge.refund.updated`
-- redelivery (a row already `failed`/`canceled` is a no-op reversal).
ALTER TABLE zeroship.refunds ADD COLUMN failed_at TIMESTAMPTZ;

-- `charge.refund.updated` carries only the Stripe Refund id (`re_…`); we resolve OUR
-- refund row through `refund_provider_refs(ref_kind='refund', external_id=re_…)` (the
-- cash money-movement ref recorded claim-after-success at issue time). This index makes
-- that reverse lookup a single keyed probe (the table's UNIQUE(provider, ref_kind,
-- external_id) already covers it, but this is the documented access path).
-- (No new index needed — the existing UNIQUE(provider, ref_kind, external_id) on
-- refund_provider_refs is the covering index for the re_… → refund_id reverse resolve.)
SELECT 1;
-- This changeset makes NO schema change (the covering index already exists), so the
-- inverse is a no-op. The explicit directive below is still required: a raw-SQL change
-- carrying no inverse directive throws RollbackFailedException on a deep rollback
-- (Liquibase cannot infer an inverse for raw SQL).

-- LOOSEN the refunds-immutable trigger (0049) to permit the TERMINAL FAILURE transition a
-- `charge.refund.updated` drives: issued|pending → failed|canceled (+ stamping failed_at).
-- The MONEY columns (amount split / destination / invoice_id / currency / idempotency
-- identity) stay FROZEN exactly as before — only the status lifecycle widens. DELETE is
-- still rejected outright. This REPLACES the 0049 function body.
--
-- Legal status transitions after this changeset:
--   pending → issued   (the original claim-after-success flip)
--   pending → failed    pending → canceled   (a refund that never issued, then bounced)
--   issued  → failed    issued  → canceled   (charge.refund.updated: the bank rejected it)
-- A terminal (failed/canceled) row NEVER moves again, and issued↛pending is forbidden.
CREATE OR REPLACE FUNCTION zeroship.refunds_immutable() RETURNS trigger AS $fn$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'refunds are append-only (no DELETE) — correct via a true-up / re-charge';
    END IF;
    -- Money + identity columns are FROZEN at claim.
    IF NEW.id <> OLD.id
       OR NEW.invoice_id <> OLD.invoice_id
       OR NEW.amount_cents <> OLD.amount_cents
       OR NEW.subtotal_cents <> OLD.subtotal_cents
       OR NEW.tax_cents <> OLD.tax_cents
       OR NEW.currency <> OLD.currency
       OR NEW.destination <> OLD.destination
       OR NEW.idempotency_key <> OLD.idempotency_key
       OR NEW.request_fingerprint <> OLD.request_fingerprint THEN
        RAISE EXCEPTION 'refund % money/identity columns are frozen — only the status lifecycle may progress', OLD.id;
    END IF;
    -- Status lifecycle: a STRICT progression. A terminal status (failed/canceled) is final;
    -- issued may only go to a terminal failure (the charge.refund.updated reversal); pending
    -- may go to issued OR a terminal failure. A same-status no-op UPDATE is allowed.
    IF NEW.status <> OLD.status THEN
        IF NOT (
               (OLD.status = 'pending' AND NEW.status IN ('issued','failed','canceled'))
            OR (OLD.status = 'issued'  AND NEW.status IN ('failed','canceled'))
        ) THEN
            RAISE EXCEPTION 'refund % status may only progress pending→issued/failed/canceled or issued→failed/canceled (got %→%)',
                OLD.id, OLD.status, NEW.status;
        END IF;
    END IF;
    RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;

-- The over-refund cap (0049) reads `Σ(refunds.amount_cents)` per destination against
-- `Σ(invoice_payments)`. A FAILED/CANCELED refund returned NO cash (the bank rejected it),
-- so it must NOT count toward the cap — otherwise a creator whose refund bounced could
-- never re-refund. EXCLUDE status IN ('failed','canceled') from both the prior-sum and the
-- (defensive) NEW.amount inclusion. This REPLACES the 0049 function body; the trigger
-- binding is unchanged. The Rust precheck (`refunds_total_for_destination`) applies the
-- SAME filter so the two agree.
CREATE OR REPLACE FUNCTION zeroship.refunds_no_over_refund() RETURNS trigger AS $fn$
DECLARE cash BIGINT; sum_cash BIGINT; sum_credit BIGINT;
BEGIN
    -- A row being INSERTed/flipped INTO a failed/canceled state never counts.
    IF NEW.status IN ('failed','canceled') THEN
        RETURN NEW;
    END IF;
    SELECT COALESCE(SUM(amount_cents), 0) INTO cash
      FROM zeroship.invoice_payments WHERE invoice_id = NEW.invoice_id;
    -- Only COUNTING refunds (not failed/canceled) consume the cap.
    SELECT COALESCE(SUM(amount_cents) FILTER (WHERE destination = 'cash'),   0),
           COALESCE(SUM(amount_cents) FILTER (WHERE destination = 'credit'), 0)
      INTO sum_cash, sum_credit
      FROM zeroship.refunds
      WHERE invoice_id = NEW.invoice_id AND id <> NEW.id
        AND status NOT IN ('failed','canceled');
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

-- A FAILED `destination='credit'` refund minted a `refund_to_credit` grant; the cash never
-- left Stripe, so that grant must be clawed back. `refund_clawback` is the NEGATIVE
-- companion that offsets the grant (one clawback per failed credit-refund), keeping the
-- balance = SUM(amount_cents) a pure ledger fact. ALTER DOMAIN to widen the membership.
ALTER DOMAIN zeroship.credit_entry_kind DROP CONSTRAINT credit_entry_kind_check;
ALTER DOMAIN zeroship.credit_entry_kind ADD CONSTRAINT credit_entry_kind_check
    CHECK (VALUE IN ('grant','promo','goodwill','refund_to_credit','consumed','void_reversal','refund_clawback'));

-- `refund_clawback` joins `consumed` as a NEGATIVE kind, and (like consumed/void_reversal)
-- it NAMES the grant it claws back via `consumed_from_grant_id`. Recreate the two CHECK
-- constraints (0048) to admit it. A clawback's `applied_invoice_id` is the refund's
-- invoice; `note` carries the refund identity (`refund_clawback:<refund_id>`), made UNIQUE
-- by a partial index below so a redelivered `charge.refund.updated` can never double-claw.
ALTER TABLE zeroship.credit_ledger DROP CONSTRAINT credit_ledger_kind_sign;
ALTER TABLE zeroship.credit_ledger ADD CONSTRAINT credit_ledger_kind_sign
    CHECK ((kind IN ('consumed','refund_clawback') AND amount_cents < 0)
        OR (kind NOT IN ('consumed','refund_clawback') AND amount_cents > 0));
ALTER TABLE zeroship.credit_ledger DROP CONSTRAINT credit_ledger_grant_ref;
ALTER TABLE zeroship.credit_ledger ADD CONSTRAINT credit_ledger_grant_ref
    CHECK ((kind IN ('consumed','void_reversal','refund_clawback') AND consumed_from_grant_id IS NOT NULL)
        OR (kind NOT IN ('consumed','void_reversal','refund_clawback') AND consumed_from_grant_id IS NULL));
-- One clawback per refund: the partial UNIQUE on the `refund_clawback:<refund_id>` note
-- makes a duplicate clawback a DB impossibility (mirrors credit_ledger_refund_to_credit_note_idx),
-- so the clawback INSERT is `ON CONFLICT DO NOTHING` (idempotent under redelivery).
CREATE UNIQUE INDEX credit_ledger_refund_clawback_note_idx
    ON zeroship.credit_ledger (note)
    WHERE kind = 'refund_clawback';

-- ─────────────────────────────────────────────────────────────────────────────
-- (2) payout.failed — a creator's connected-account payout bounced
-- ─────────────────────────────────────────────────────────────────────────────

-- A payout to a creator's connected account FAILED (`payout.failed`). NOT folded into the
-- `payouts` ledger (whose tight CHECKs — net = gross − fee ≥ 0 — model a SUCCESSFUL revenue
-- payout, not a failure): a failure is its OWN append-only fact. The notify cron scans this
-- table for the `payout_failed` notification. Idempotent on the Stripe payout id (`po_…`):
-- a redelivered `payout.failed` for the same `provider_payout_id` is a no-op.
--
-- creator_id FKs `creator_accounts(creator_id)` (the Connect identity that owns the failed
-- payout), ON DELETE CASCADE so unlinking the account cleans up. `id` is a `pof_…` typed id
-- minted in Rust (no SQL DEFAULT — no in-DB base62 generator) and is the notify
-- `transition_id` for the `payout_failed` kind (prefix disjoint from every other source).
CREATE TABLE zeroship.payout_failures (
    id                  TEXT        PRIMARY KEY,                -- pof_<base62> (disjoint prefix)
    creator_id          UUID        NOT NULL REFERENCES zeroship.creator_accounts(creator_id) ON DELETE CASCADE,
    -- The Stripe payout id (`po_…`). UNIQUE ⇒ one row per failed payout; a redelivered
    -- `payout.failed` for the same payout is an idempotent no-op.
    provider_payout_id  TEXT        NOT NULL,
    -- The connected account (`acct_…`) the payout was destined for — resolved from the
    -- Connect event's top-level `account` (verified docs.stripe.com/connect/webhooks: each
    -- Connect event carries a top-level `account` identifying the connected account).
    stripe_account_id   TEXT        NOT NULL,
    amount_cents        BIGINT      NOT NULL CHECK (amount_cents >= 0),
    currency            CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    failure_code        TEXT,                                   -- Stripe payout.failure_code (account_closed/…)
    failure_message     TEXT,                                   -- Stripe payout.failure_message (human-readable)
    occurred_at         TIMESTAMPTZ NOT NULL,                   -- the Stripe event.created
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (provider_payout_id)
);
CREATE INDEX payout_failures_creator_idx ON zeroship.payout_failures (creator_id, created_at DESC);

-- Append-only: a payout-failure is a permanent fact (no UPDATE/DELETE), uniform with the
-- ledger immutability discipline. A correction is a NEW row, never an edit.
CREATE FUNCTION zeroship.payout_failures_immutable() RETURNS trigger AS $fn$
BEGIN
    RAISE EXCEPTION 'payout_failures is append-only (no UPDATE/DELETE)';
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER payout_failures_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.payout_failures
    FOR EACH ROW EXECUTE FUNCTION zeroship.payout_failures_immutable();

-- ─────────────────────────────────────────────────────────────────────────────
-- (3) payment_intent.payment_failed — an end-user's Connect checkout charge failed
-- ─────────────────────────────────────────────────────────────────────────────

-- An end-user's Connect checkout PaymentIntent FAILED (`payment_intent.payment_failed`). No
-- money moved — this is INFORMATIONAL (the front-end already saw it via the client_secret
-- confirm) — but it must be SURFACED, not silently dropped. The notify cron scans this for
-- the `checkout_failed` notification. Idempotent on the PaymentIntent id (`pi_…`).
--
-- creator_id FKs `creator_accounts(creator_id)` — the Connect creator whose checkout failed,
-- resolved from the PI's `transfer_data.destination`/`on_behalf_of` (a destination charge on
-- the platform) OR the Connect event's top-level `account` (a direct charge on the connected
-- account). `id` is a `cof_…` typed id (the notify `transition_id`).
CREATE TABLE zeroship.connect_checkout_failures (
    id                      TEXT        PRIMARY KEY,            -- cof_<base62> (disjoint prefix)
    creator_id              UUID        NOT NULL REFERENCES zeroship.creator_accounts(creator_id) ON DELETE CASCADE,
    -- The Stripe PaymentIntent id (`pi_…`). UNIQUE ⇒ one row per failed PI; a redelivery is
    -- an idempotent no-op.
    provider_payment_intent_id TEXT     NOT NULL,
    stripe_account_id       TEXT        NOT NULL,               -- the creator's acct_…
    amount_cents            BIGINT      NOT NULL CHECK (amount_cents >= 0),
    currency                CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    failure_code            TEXT,                               -- last_payment_error.code (card_declined/…)
    failure_message         TEXT,                               -- last_payment_error.message
    occurred_at             TIMESTAMPTZ NOT NULL,               -- the Stripe event.created
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (provider_payment_intent_id)
);
CREATE INDEX connect_checkout_failures_creator_idx ON zeroship.connect_checkout_failures (creator_id, created_at DESC);

CREATE FUNCTION zeroship.connect_checkout_failures_immutable() RETURNS trigger AS $fn$
BEGIN
    RAISE EXCEPTION 'connect_checkout_failures is append-only (no UPDATE/DELETE)';
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER connect_checkout_failures_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.connect_checkout_failures
    FOR EACH ROW EXECUTE FUNCTION zeroship.connect_checkout_failures_immutable();

-- ─────────────────────────────────────────────────────────────────────────────
-- Notification kinds for the two new creator-facing events
-- ─────────────────────────────────────────────────────────────────────────────

-- `payout_failed` (the creator's payout bounced) + `checkout_failed` (an end-user's checkout
-- charge failed) join the notification-kind domain (0052). ALTER DOMAIN to widen membership.
ALTER DOMAIN zeroship.billing_notification_kind DROP CONSTRAINT billing_notification_kind_check;
ALTER DOMAIN zeroship.billing_notification_kind ADD CONSTRAINT billing_notification_kind_check
    CHECK (VALUE IN ('payment_failed','past_due','suspended','recovered',
                     'invoice_finalized','refunded','disputed',
                     'payout_failed','checkout_failed',
                     'spend_warn','spend_degrade','spend_block'));

-- ─────────────────────────────────────────────────────────────────────────────
-- Grants
-- ─────────────────────────────────────────────────────────────────────────────

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- refunds already has SELECT/INSERT/UPDATE (0049) — the failed/canceled flip is an UPDATE.
    -- The new failure ledgers are append-only: SELECT/INSERT only (the immutability triggers
    -- are the backstop; the missing UPDATE/DELETE grant is the first line of defence).
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.payout_failures            TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.connect_checkout_failures  TO zeroship_control';
  END IF;
END $g$;
