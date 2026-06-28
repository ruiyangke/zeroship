-- Identity only; stripe_customer_id MOVES to billing_customer_refs (REAL FK).
-- Creator(user)-keyed control bookkeeping ⇒ no app RLS.
-- creator_id STAYS ON DELETE CASCADE from users (uniform with the existing erase
-- model). account_reaper.rs retains the users row (anonymize-in-place) whenever the
-- creator has financial history, so the FK target survives and CASCADE never fires for
-- an ever-billed creator; a never-billed creator is hard-deleted and CASCADE cleanly
-- reaps this empty shell. The erase tombstone lives on users.anonymized_at — NOT here.
CREATE TABLE zeroship.creator_billing (
    creator_id     UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE CASCADE,
    default_pm_set BOOLEAN NOT NULL DEFAULT false,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Customer↔provider mapping: a NARROW table with a REAL FK to creator_billing. A
-- stray/buggy writer CANNOT orphan a customer ref. One creator holds one ref PER
-- provider ⇒ PK (creator_id, provider); external_id globally unique per provider ⇒
-- UNIQUE (provider, external_id) backs the reverse lookup.
CREATE TABLE zeroship.billing_customer_refs (
    creator_id  UUID NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    -- 'stripe' (the platform Customer — shared by the Native invoice rail AND
    -- Stripe Billing Meters, which posts meter events against the same cus_…) |
    -- 'openmeter' (a distinct external customer handle). Stripe-Meters does NOT
    -- get its own ref; see Key flow E.
    provider    TEXT NOT NULL,
    external_id TEXT NOT NULL,                -- cus_… (stripe) / external handle (openmeter)
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (creator_id, provider),
    -- Reverse lookup: the webhook resolves creator FROM a customer id with NO
    -- provider in hand (stripe_handlers.rs:874 get_creator_by_customer(customer)).
    -- A provider customer id (cus_…) is globally unique, so the reverse lookup is
    -- `WHERE external_id = $1` and a STANDALONE UNIQUE(external_id) makes it
    -- constraint-guaranteed-singular — NOT merely UNIQUE(provider, external_id),
    -- which would not back a providerless probe. UNIQUE(provider, external_id)
    -- additionally documents per-provider scoping but is subsumed by the stricter
    -- global unique below.
    UNIQUE (external_id)
);

-- Provider-agnostic per-(creator, period) claim. UNIQUE(creator_id, period) == old
-- billing_runs PK (the no-double-bill claim). status carries the short-circuit
-- (finalized ⇒ skip), replacing "stripe_invoice_id NOT NULL".
CREATE TABLE zeroship.invoices (
    id             TEXT                    PRIMARY KEY,           -- inv_<base62>
    creator_id     UUID                    NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    period         zeroship.billing_period NOT NULL,
    status         zeroship.invoice_status NOT NULL DEFAULT 'draft',
    currency       CHAR(3)                 NOT NULL DEFAULT 'usd' CHECK (currency ~ '^[a-z]{3}$'),
    subtotal_cents BIGINT                  NOT NULL DEFAULT 0 CHECK (subtotal_cents >= 0),
    credit_cents   BIGINT                  NOT NULL DEFAULT 0 CHECK (credit_cents   >= 0),  -- 0 at launch; shape ready
    tax_cents      BIGINT                  NOT NULL DEFAULT 0 CHECK (tax_cents       >= 0),  -- 0 at launch
    total_cents    BIGINT                  NOT NULL DEFAULT 0 CHECK (total_cents     >= 0),
    finalized_at   TIMESTAMPTZ,
    voided_at      TIMESTAMPTZ,
    created_at     TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    CONSTRAINT invoice_total_balances CHECK (total_cents = subtotal_cents - credit_cents + tax_cents)
    -- VOID + REISSUE (gap #26 C, billing-ops PR-1). The no-double-bill claim is NOT an
    -- unconditional UNIQUE(creator_id, period) — that would PERMANENTLY block reissuing a
    -- corrected invoice for a period whose first invoice was voided (a void is the legal
    -- correction transition; once voided an invoice must RELEASE its period claim so a
    -- fresh re-priced invoice can take the slot). The claim is a PARTIAL unique index
    -- `WHERE status <> 'void'` (created below): AT MOST ONE non-void invoice per
    -- (creator, period), UNBOUNDED void rows (the audit trail). The reconciler's
    -- `ON CONFLICT (creator_id, period) WHERE status <> 'void' DO NOTHING` targets it.
);
-- PARTIAL UNIQUE PERIOD CLAIM (gap #26 C, billing-ops PR-1). See the note on the table.
CREATE UNIQUE INDEX invoices_active_period_claim
    ON zeroship.invoices (creator_id, period)
    WHERE status <> 'void';

-- Per-app line. PK (invoice_id, app_id) == billing_run_items grain. The SNAPSHOT block
-- is the reproducibility fix. app_id gets the FK it lacked, RESTRICT.
-- total_units/billable_units are DERIVED at read (single source of truth), NOT stored —
-- pricing.rs floors PER METRIC before summing, so a stored copy could silently disagree.
CREATE TABLE zeroship.invoice_lines (
    invoice_id             TEXT   NOT NULL REFERENCES zeroship.invoices(id) ON DELETE RESTRICT,
    app_id                 UUID   NOT NULL REFERENCES zeroship.apps(id)     ON DELETE RESTRICT,
    included_units         BIGINT NOT NULL CHECK (included_units >= 0),  -- APPLIED quota (frozen)
    fx_pico_cents_per_unit BIGINT NOT NULL CHECK (fx_pico_cents_per_unit >= 1000),  -- RESOLVED FX charged
    base_fee_cents         BIGINT NOT NULL DEFAULT 0 CHECK (base_fee_cents >= 0),
    amount_cents           BIGINT NOT NULL CHECK (amount_cents   >= 0),  -- ChargeBreakdown.total_cents (authoritative)
    usage_snapshot         JSONB  NOT NULL,                              -- {metric: raw_total} (SOURCE OF TRUTH)
    weights_snapshot       JSONB  NOT NULL,                              -- {metric:{units_per_op,per_units}} applied
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (invoice_id, app_id)
);

-- INVOICE-level provider ids. Real FK → invoices(id): no dangling ref. Core invoices
-- carry no provider ids.
--
-- DISPUTE-RESOLUTION LINKAGE (billing-ops gap #26, PR-8 CRITICAL-1). A Stripe Dispute
-- object carries NO `invoice` field — only `charge` (ch_…) and `payment_intent` (pi_…)
-- (docs.stripe.com/api/disputes/object). To map a dispute back to OUR invoice, the
-- `invoice.paid` handler captures the paid Invoice's `pi_…`/`ch_…` and persists them here
-- as ADDITIONAL `ref_kind`s alongside the finalize-time `'invoice'` (in_…) ref:
--   'invoice'        — the finalized Stripe invoice id (in_…), written at finalize.
--   'draft_invoice'  — the draft id, written while building the invoice.
--   'payment_intent' — the paid invoice's PaymentIntent (pi_…), written at invoice.paid.
--   'charge'         — the paid invoice's Charge (ch_…), written at invoice.paid.
-- A pi_/ch_ is GLOBALLY unique at Stripe, so the existing UNIQUE(provider, ref_kind,
-- external_id) makes dispute resolution deterministic (no LIMIT-1 ambiguity). `ref_kind`
-- is a plain TEXT column (no constrained domain), so adding these kinds needs no schema
-- change beyond this documentation — the comment is the contract.
CREATE TABLE zeroship.billing_provider_refs (
    invoice_id  TEXT NOT NULL REFERENCES zeroship.invoices(id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,                -- 'stripe' | 'stripe_meters' | 'openmeter'
    ref_kind    TEXT NOT NULL,                -- 'invoice' | 'draft_invoice' | 'payment_intent' | 'charge'
    external_id TEXT NOT NULL,                -- in_… | pi_… | ch_…
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (invoice_id, provider, ref_kind),
    UNIQUE (provider, ref_kind, external_id)
);

-- LINE-level provider ids: a REAL COMPOSITE FK → invoice_lines(invoice_id, app_id),
-- replacing an asserted-not-enforced '<inv>:<app>' TEXT object_id. A writer CANNOT
-- insert a line ref for a non-existent (invoice, app) line — the DB rejects it, so the
-- per-app double-bill guard ("an invoice_item ref EXISTS for this line") can never
-- silently fail-to-match a malformed key. Composite PK ⇒ no cross-period collision.
CREATE TABLE zeroship.billing_line_provider_refs (
    invoice_id  TEXT NOT NULL,
    app_id      UUID NOT NULL,
    provider    TEXT NOT NULL,                -- 'stripe' | 'stripe_meters' | 'openmeter'
    ref_kind    TEXT NOT NULL,                -- 'invoice_item'
    external_id TEXT NOT NULL,                -- ii_…
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (invoice_id, app_id, provider, ref_kind),
    UNIQUE (provider, ref_kind, external_id),
    -- NAMED (billing-ops PR-1, MINOR-7): the composite FK is given an EXPLICIT name so a
    -- later changeset (0054, full usage-segment proration) can DROP it by a KNOWN name
    -- rather than a Postgres-guessed one. The DROP is name-pinned (no IF EXISTS), so a
    -- name skew fails the migration LOUDLY rather than silently leaving the old 2-col FK.
    CONSTRAINT billing_line_provider_refs_line_fk
        FOREIGN KEY (invoice_id, app_id)
        REFERENCES zeroship.invoice_lines(invoice_id, app_id) ON DELETE CASCADE
);

-- Once finalized, the only legal change is finalized → void. draft→finalized is
-- IMPLICITLY allowed. The finalize UPDATE writes subtotal/credit/tax/total in ONE
-- statement, so the balance CHECK never sees a half-written row.
CREATE FUNCTION zeroship.invoices_immutable() RETURNS trigger AS $fn$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'invoices are append-only (no DELETE)';
    END IF;
    IF OLD.status = 'finalized' THEN
        IF NEW.status = 'void'
           AND NEW.id = OLD.id AND NEW.creator_id = OLD.creator_id
           AND NEW.period = OLD.period
           AND NEW.subtotal_cents = OLD.subtotal_cents
           AND NEW.credit_cents = OLD.credit_cents
           AND NEW.tax_cents = OLD.tax_cents
           AND NEW.total_cents = OLD.total_cents THEN
            RETURN NEW;
        END IF;
        RAISE EXCEPTION 'invoice % is finalized — only the void transition is permitted', OLD.id;
    END IF;
    RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER invoices_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.invoices
    FOR EACH ROW EXECUTE FUNCTION zeroship.invoices_immutable();

-- The reproducibility record lives on the LINES. Freeze a line whenever its PARENT
-- invoice is finalized: reject UPDATE/DELETE. While the parent is draft, lines stay
-- mutable (the reconciler builds them).
-- CONCURRENCY: line mutability is SAFE because the per-creator reconcile is SINGLE-
-- FLIGHTED by an advisory lock — the same creator's lines and finalize never run
-- concurrently. A future NON-cron line writer MUST take the same per-creator advisory
-- lock or SELECT … FOR UPDATE the parent invoice first. The trigger is the backstop;
-- the advisory lock is the ordering guarantee.
CREATE FUNCTION zeroship.invoice_lines_immutable() RETURNS trigger AS $fn$
DECLARE parent_status zeroship.invoice_status;
BEGIN
    SELECT status INTO parent_status FROM zeroship.invoices
        WHERE id = COALESCE(OLD.invoice_id, NEW.invoice_id);
    IF parent_status = 'finalized' THEN
        RAISE EXCEPTION 'invoice_line for invoice % is frozen — parent invoice is finalized',
            COALESCE(OLD.invoice_id, NEW.invoice_id);
    END IF;
    RETURN COALESCE(NEW, OLD);
END;
$fn$ LANGUAGE plpgsql;
-- Schema MAJOR-2: fire on INSERT too, not only UPDATE/DELETE — otherwise a NEW
-- line could be appended to an ALREADY-finalized invoice (the reproducibility
-- record is supposed to be frozen at finalize). The function COALESCE(OLD, NEW)s
-- the invoice_id, so an INSERT (OLD is NULL) reads the parent via NEW and is
-- rejected when that parent is finalized; a draft-invoice INSERT still passes.
CREATE TRIGGER invoice_lines_immutable_trg
    BEFORE INSERT OR UPDATE OR DELETE ON zeroship.invoice_lines
    FOR EACH ROW EXECUTE FUNCTION zeroship.invoice_lines_immutable();

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.creator_billing            TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.billing_customer_refs      TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.invoices                   TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE         ON zeroship.invoice_lines              TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.billing_provider_refs      TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT                 ON zeroship.billing_line_provider_refs TO zeroship_control';
  END IF;
END $g$;
