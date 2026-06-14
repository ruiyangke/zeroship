--liquibase formatted sql

-- CREDITS (billing-ops gap #26, PR-2; design `0048 credit_ledger`). The Stripe
-- customer-balance model: an APPEND-ONLY ledger whose BALANCE is SUM(amount_cents),
-- NEVER a stored column (it cannot drift from its history). A grant is a POSITIVE
-- entry; at finalize the reconciler appends ONE NEGATIVE `consumed` entry PER DRAWN
-- GRANT (design MAJOR-2 — per-grant, not a single aggregate companion, so
-- credit-expiry attribution is exact). Balance for a creator = SUM(amount_cents) >= 0.
-- That non-negativity is intrinsic to the consume operation: `consume_at_finalize`
-- takes a TRANSACTION-SCOPED per-creator advisory lock (`pg_advisory_xact_lock(
-- hashtext(creator_id::text)::bigint)`) as its first act, so two concurrent consumes
-- for one creator SERIALIZE — each reads `remaining` and draws under the lock, and a
-- grant can never be over-drawn. The guarantee therefore does NOT depend on any outer
-- fleet-wide sweep lock (which an on-demand finalizer would not hold); the consume op
-- self-serializes per creator and draws at most the balance, filtered by currency.
--
-- RLS posture: creator-keyed control bookkeeping ⇒ NO app FORCE RLS (control runs
-- BYPASSRLS; the key is a creator_id, not a tenant app_id) — uniform with
-- creator_billing / invoices / invoice_payments. Real FK→creator_billing(creator_id)
-- ON DELETE CASCADE ⇒ a stray writer cannot orphan an entry; CASCADE is uniform with
-- the redesign's erase model (anonymize-retained creators keep the FK target alive;
-- never-billed creators CASCADE clean).
--
-- USD-pinned v1: `currency` defaults 'usd'; the Rust grant boundary REJECTS a non-USD
-- currency, and the consume query filters `AND currency = invoice.currency` so a stray
-- non-USD grant can never be drawn against a USD bill (design MINOR-5).

--changeset zeroship:credit-entry-kind-domain splitStatements:true
-- Entry kinds: membership only (the DOMAIN encodes the SET; the kind↔sign coupling
-- is a TABLE CHECK below, design MAJOR-1). 'consumed' is the negative companion.
-- 'refund_to_credit' is the destination of a refund routed to balance (PR-3).
-- 'void_reversal' is the compensating positive entry restoring credit a voided
-- invoice consumed (PR-1 void+reissue, CRITICAL-2). Authored BEFORE the table that
-- references it (domains before tables, mirroring 0037 / 0046).
CREATE DOMAIN zeroship.credit_entry_kind AS TEXT
    CHECK (VALUE IN ('grant','promo','goodwill','refund_to_credit','consumed','void_reversal'));
--rollback DROP DOMAIN IF EXISTS zeroship.credit_entry_kind;

--changeset zeroship:credit-ledger splitStatements:true
CREATE TABLE zeroship.credit_ledger (
    id          TEXT        PRIMARY KEY,                -- crd_<base62> (disjoint 3-char prefix)
    creator_id  UUID        NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    -- entry provenance. Positive kinds GRANT balance; 'consumed' is the negative
    -- companion written PER DRAWN GRANT at finalize. Membership only; ordering in Rust.
    kind        zeroship.credit_entry_kind NOT NULL,
    -- SIGNED: all positive kinds > 0; consumed < 0. The sign convention is what makes
    -- the balance a pure SUM. The convention is ENFORCED by a CHECK coupling kind↔sign
    -- (below, design MAJOR-1), not only by a test.
    amount_cents BIGINT     NOT NULL CHECK (amount_cents <> 0),
    currency    CHAR(3)     NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    -- The invoice a 'consumed'/'void_reversal'/'refund_to_credit' entry relates to
    -- (NULL for grants). Real FK ⇒ such an entry can never reference a non-existent
    -- invoice; the audit join is DB-guaranteed. The ON DELETE RESTRICT is NOT
    -- load-bearing — invoices are append-only and NEVER deleted (their own immutability
    -- trigger rejects DELETE) — kept only as a uniform, defensive default.
    applied_invoice_id TEXT REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    -- PER-GRANT consume (design MAJOR-2 + DECISION 7). A 'consumed' entry draws from
    -- exactly ONE grant, named here, so credit-expiry attribution is EXACT. A
    -- void_reversal references the consumed grant it restores via the same column.
    -- Real self-FK to the granting row; NULL for grant kinds.
    consumed_from_grant_id TEXT REFERENCES zeroship.credit_ledger(id) ON DELETE RESTRICT,
    -- Optional expiry (DECISION 7): a grant past expires_at is NOT consumable. NULL ⇒
    -- never expires. The reconciler's consume-query filters
    -- `expires_at IS NULL OR expires_at > NOW()` and draws grant-by-grant oldest-first.
    expires_at  TIMESTAMPTZ,
    note        TEXT,                                   -- operator audit ('promo X', 'goodwill ticket #…')
    -- Operator-supplied key on GRANT-class entries written via POST /billing/credit. A
    -- double-clicked grant carries the same key, so the partial UNIQUE index below makes
    -- the second INSERT a no-op (no double grant). NULL for reconciler-internal entries
    -- (consumed / void_reversal / refund_to_credit are guarded by their own claim paths),
    -- so the column is nullable and the UNIQUE is PARTIAL.
    idempotency_key TEXT,
    -- Request-body fingerprint, same posture as refunds (design MINOR-A). A grant key
    -- reused with a DIFFERENT amount/currency/expiry must NOT silently return the first
    -- grant (Stripe 400s on key-reuse-with-different-body). The endpoint stores a SHA-256
    -- over the canonical grant request here; on a key hit it returns 409 unless the
    -- fingerprint matches. NULL whenever idempotency_key is NULL.
    request_fingerprint TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- kind↔sign coupling (design MAJOR-1). consumed is the ONLY negative kind; every
    -- other kind (grant/promo/goodwill/refund_to_credit/void_reversal) is positive.
    -- This makes the SUM-balance invariant a DB fact, not a test convention.
    CONSTRAINT credit_ledger_kind_sign
        CHECK ((kind = 'consumed' AND amount_cents < 0)
            OR (kind <> 'consumed' AND amount_cents > 0)),
    -- A consumed/void_reversal entry MUST name the grant it draws from / restores;
    -- a grant-class entry MUST NOT (it is itself a source).
    CONSTRAINT credit_ledger_grant_ref
        CHECK ((kind IN ('consumed','void_reversal') AND consumed_from_grant_id IS NOT NULL)
            OR (kind NOT IN ('consumed','void_reversal') AND consumed_from_grant_id IS NULL))
);
-- The hot read is "this creator's consumable balance, oldest-first" (FIFO consume).
CREATE INDEX credit_ledger_creator_created_idx
    ON zeroship.credit_ledger (creator_id, created_at);
-- Operator-grant idempotency. PARTIAL unique (only grant-class entries carry a key)
-- so a retried POST /billing/credit is a no-op.
CREATE UNIQUE INDEX credit_ledger_idempotency_key_idx
    ON zeroship.credit_ledger (idempotency_key)
    WHERE idempotency_key IS NOT NULL;
--rollback DROP INDEX IF EXISTS zeroship.credit_ledger_idempotency_key_idx;
--rollback DROP INDEX IF EXISTS zeroship.credit_ledger_creator_created_idx;
--rollback DROP TABLE zeroship.credit_ledger;

--changeset zeroship:credit-ledger-immutable splitStatements:false
-- A credit entry is append-only: a balance correction is a NEW offsetting entry,
-- never an edit (uniform with the invoice/line/payment immutability discipline).
-- Reject UPDATE and DELETE outright.
CREATE FUNCTION zeroship.credit_ledger_immutable() RETURNS trigger AS $fn$
BEGIN
    RAISE EXCEPTION 'credit_ledger is append-only (no UPDATE/DELETE) — correct via a new offsetting entry';
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER credit_ledger_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.credit_ledger
    FOR EACH ROW EXECUTE FUNCTION zeroship.credit_ledger_immutable();
--rollback DROP TRIGGER IF EXISTS credit_ledger_immutable_trg ON zeroship.credit_ledger;
--rollback DROP FUNCTION IF EXISTS zeroship.credit_ledger_immutable();

--changeset zeroship:credit-ledger-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- INSERT-only beyond SELECT: append-only, no UPDATE/DELETE grant (the trigger
    -- is the backstop; the grant is the first line of defence — least privilege).
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.credit_ledger TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.credit_ledger FROM zeroship_control'; END IF; END $rb$;
