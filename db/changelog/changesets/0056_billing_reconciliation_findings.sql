--liquibase formatted sql

-- STRIPE STATE-RECONCILIATION FINDINGS (#28 Stripe reconciliation). The production
-- BACKSTOP that catches drift when Stripe webhooks are missed / dropped / out-of-order.
-- A periodic cron (`crates/control/src/cron/stripe_reconcile.rs`) compares OUR billing
-- state (invoices / refunds / disputes) against Stripe over a bounded recent window and
-- APPENDS a finding row here for each drift it detects. It is READ-ONLY w.r.t. money by
-- default: it detects + records + alerts, it does NOT auto-correct cash on a transient
-- Stripe read (operator-reviewed; the one conservatively-safe exception — parking a
-- fully-missed dispute — is gated behind an OFF-by-default config flag).
--
-- APPEND-ONLY + IDEMPOTENT. Re-running a sweep must NOT record the same drift twice. The
-- dedup is a UNIQUE `dedup_key` (a stable, value-free fingerprint of the drift identity:
-- `kind:entity_id:our_value_hash:stripe_value_hash` chosen by the cron) — a second sweep
-- that re-observes the SAME drift `INSERT … ON CONFLICT (dedup_key) DO NOTHING` no-ops.
-- A drift that CHANGES (e.g. our value advances toward Stripe's) yields a NEW dedup_key
-- and a NEW finding, so progress/regression is itself an observable trail. Once an
-- operator resolves a drift it is stamped `resolved_at` (the only legal mutation) — the
-- row is never deleted (a permanent audit fact, uniform with invoice_payments /
-- billing_disputes immutability).
--
-- RLS posture: control-internal, creator/invoice-keyed (NOT app-keyed) ⇒ NO app FORCE
-- RLS, uniform with invoices / invoice_payments / refunds / billing_disputes. Control
-- runs BYPASSRLS; the row is keyed by a Stripe entity id, not a tenant app_id.
--
-- CHANGELOG ORDER: after 0055 (lexicographic, includeAll). FK-free on purpose — the
-- finding references an entity by its Stripe-or-internal id as plain TEXT so it can record
-- a drift even for an entity we have NO internal row for (case 3: a fully-missed dispute
-- with neither a billing_disputes nor a pending_disputes row). A hard FK would make that
-- exact "we are missing the row" finding impossible to record.

--changeset zeroship:billing-reconciliation-finding-kind splitStatements:true
-- The finding taxonomy, membership only. Authored BEFORE the table that references it
-- (domains before tables, mirroring 0037 / 0053).
--   'missed_invoice_payment' — Stripe says the invoice is PAID (amount_paid > 0) but we
--       have NO charge row in invoice_payments (a missed `invoice.paid`).
--   'invoice_status_drift'   — our invoice.status / total_cents disagrees with Stripe's
--       status / amount_due (a missed finalize / void / uncollectible transition).
--   'refund_status_drift'    — Stripe says the refund failed/canceled but we still have it
--       pending/issued (a missed `charge.refund.updated` reversal).
--   'dispute_status_drift'   — our dispute.status / amount disagrees with Stripe's (a
--       missed `charge.dispute.updated`/`.closed`).
--   'missing_dispute'        — Stripe has a dispute in the window we have NEITHER a
--       billing_disputes NOR a pending_disputes row for (a fully-missed
--       `charge.dispute.created` — the dispute-before-linkage backstop).
CREATE DOMAIN zeroship.reconciliation_finding_kind AS TEXT
    CHECK (VALUE IN (
        'missed_invoice_payment',
        'invoice_status_drift',
        'refund_status_drift',
        'dispute_status_drift',
        'missing_dispute'
    ));
--rollback DROP DOMAIN IF EXISTS zeroship.reconciliation_finding_kind;

--changeset zeroship:billing-reconciliation-finding-severity splitStatements:true
-- Finding severity, membership only. 'high' drives a loud log + (optionally) an operator
-- alert; 'low'/'medium' are recorded for the audit trail / dashboard.
CREATE DOMAIN zeroship.reconciliation_finding_severity AS TEXT
    CHECK (VALUE IN ('low','medium','high'));
--rollback DROP DOMAIN IF EXISTS zeroship.reconciliation_finding_severity;

--changeset zeroship:billing-reconciliation-findings splitStatements:true
CREATE TABLE zeroship.billing_reconciliation_findings (
    id          TEXT PRIMARY KEY,                            -- rcf_<base62> (disjoint 3-char prefix)
    kind        zeroship.reconciliation_finding_kind     NOT NULL,
    severity    zeroship.reconciliation_finding_severity NOT NULL DEFAULT 'medium',
    -- The entity the drift is about, as a STABLE provider-or-internal id string:
    --   * an invoice finding → the Stripe in_… (the provider ref the sweep fetched).
    --   * a refund finding   → the Stripe re_… (the provider ref the sweep fetched).
    --   * a dispute finding  → the Stripe du_… (always provider-originated).
    -- TEXT (not a typed FK): the whole point of `missing_dispute` is that we have NO
    -- internal row, so a FK would make that finding impossible.
    entity_id   TEXT NOT NULL,
    -- OUR observed state + STRIPE's observed state, as JSON (the exact mismatched fields,
    -- e.g. {"status":"issued"} vs {"status":"failed"}). Operator-reviewable; the shape is
    -- finding-kind-specific and intentionally schema-free.
    our_value    JSONB,
    stripe_value JSONB,
    -- IDEMPOTENCY (re-running a sweep must not duplicate findings). A stable, value-free
    -- fingerprint of the drift identity chosen by the cron (kind + entity + a hash of the
    -- compared values), so the SAME drift re-observed next sweep collides here and the
    -- INSERT … ON CONFLICT (dedup_key) DO NOTHING no-ops. A CHANGED drift gets a fresh key
    -- ⇒ a fresh finding (the progress/regression trail).
    dedup_key   TEXT NOT NULL,
    detected_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Operator-resolution stamp. The ONLY legal mutation (set once, NULL→a timestamp); the
    -- row is otherwise frozen + never deleted (a permanent audit fact).
    resolved_at TIMESTAMPTZ,
    UNIQUE (dedup_key)
);
CREATE INDEX billing_reconciliation_findings_kind_idx
    ON zeroship.billing_reconciliation_findings (kind, detected_at);
-- Open (unresolved) findings are the operator's working set — a partial index keeps that
-- scan tight as the audit history grows.
CREATE INDEX billing_reconciliation_findings_open_idx
    ON zeroship.billing_reconciliation_findings (detected_at)
    WHERE resolved_at IS NULL;
--rollback DROP INDEX IF EXISTS zeroship.billing_reconciliation_findings_open_idx;
--rollback DROP INDEX IF EXISTS zeroship.billing_reconciliation_findings_kind_idx;
--rollback DROP TABLE zeroship.billing_reconciliation_findings;

--changeset zeroship:billing-reconciliation-findings-controlled-immutable splitStatements:false
-- A finding is append-OR-controlled-update only: the drift facts are FROZEN at insert
-- (kind / entity_id / our_value / stripe_value / dedup_key / detected_at NEVER change),
-- and the ONLY legal mutation is stamping `resolved_at` once (NULL → a timestamp). DELETE
-- is rejected outright (uniform with invoice_payments / billing_disputes immutability).
-- This blocks a stray writer from rewriting a recorded drift or un-resolving a finding.
CREATE FUNCTION zeroship.billing_reconciliation_findings_controlled_update() RETURNS trigger AS $fn$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'billing_reconciliation_findings is append-only (no DELETE) — a finding is a permanent audit fact';
    END IF;
    IF NEW.id <> OLD.id
       OR NEW.kind <> OLD.kind
       OR NEW.entity_id <> OLD.entity_id
       OR NEW.dedup_key <> OLD.dedup_key
       OR NEW.detected_at <> OLD.detected_at
       OR NEW.our_value IS DISTINCT FROM OLD.our_value
       OR NEW.stripe_value IS DISTINCT FROM OLD.stripe_value THEN
        RAISE EXCEPTION 'billing_reconciliation_findings frozen columns are immutable — only resolved_at/severity may change';
    END IF;
    -- resolved_at is set-once: NULL → a timestamp. A resolved finding never un-resolves.
    IF OLD.resolved_at IS NOT NULL AND NEW.resolved_at IS DISTINCT FROM OLD.resolved_at THEN
        RAISE EXCEPTION 'billing_reconciliation_findings.resolved_at is set-once (a resolved finding stays resolved)';
    END IF;
    RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER billing_reconciliation_findings_controlled_update_trg
    BEFORE UPDATE OR DELETE ON zeroship.billing_reconciliation_findings
    FOR EACH ROW EXECUTE FUNCTION zeroship.billing_reconciliation_findings_controlled_update();
--rollback DROP TRIGGER IF EXISTS billing_reconciliation_findings_controlled_update_trg ON zeroship.billing_reconciliation_findings;
--rollback DROP FUNCTION IF EXISTS zeroship.billing_reconciliation_findings_controlled_update();

--changeset zeroship:billing-reconciliation-findings-grants splitStatements:false rollbackSplitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- SELECT/INSERT (the sweep appends) + UPDATE (operator resolution stamp; the
    -- controlled-update trigger bounds WHAT may change). NO DELETE (append-only).
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_reconciliation_findings TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_reconciliation_findings FROM zeroship_control'; END IF; END $rb$;
