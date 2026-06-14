--liquibase formatted sql

--changeset zeroship:invoice-lines-segment-reshape splitStatements:true
-- LINE-GRAIN RESHAPE (round 3, decision 1 OVERRIDE — full usage-segment proration).
-- The redesign's invoice_lines PK is (invoice_id, app_id): ONE line per app per invoice.
-- Full usage-segment proration needs MULTIPLE lines per app per invoice — one per plan
-- SEGMENT (an app with N plan-change events in the period has N+1 segments). Add a
-- `segment_no SMALLINT` discriminator to the PK so (invoice_id, app_id, segment_no) is
-- unique: segment_no = 0 is the first (or only) segment, 1 the next, … in effective_at
-- order. Each segment line freezes its OWN plan's snapshot so it replays bit-for-bit.
--
-- The doc numbers this changeset `0054`; the actual billing changesets landed at
-- 0046 (invoice_payments) / 0048 (credits) / 0049 (refunds), so the next free slot
-- after the 0042 redesign tables this ALTERs is 0050 (plan_change_events) + 0051 (this).
-- Liquibase applies in lexicographic includeAll order, so 0051 lands AFTER 0042 (the
-- tables it ALTERs were created there) — the doc's changelog-ORDER requirement holds.
--
-- NOTE the segment_no default 0: every existing/no-change line is segment 0, so the
-- no-change path (0 plan-change events) produces exactly ONE line per app — segment_no=0,
-- full-period usage, no proration — byte-for-byte identical to today. The reshape
-- DEGENERATES cleanly.

-- 1. Drop the composite FK on billing_line_provider_refs FIRST (it references the PK we
--    are about to widen). round 4, MINOR-7: the FK is EXPLICITLY NAMED in 0042 —
--    `billing_line_provider_refs_line_fk` — so this DROPs a KNOWN name, not a Postgres-
--    guessed one. The DROP is name-pinned (no IF EXISTS), so a name skew fails the
--    migration LOUDLY rather than silently leaving the old 2-col FK in place (which would
--    let a per-segment ref point at a non-existent line).
ALTER TABLE zeroship.billing_line_provider_refs
    DROP CONSTRAINT billing_line_provider_refs_line_fk;

-- 2. Widen invoice_lines: add segment_no, re-key the PK to include it.
ALTER TABLE zeroship.invoice_lines
    ADD COLUMN segment_no SMALLINT NOT NULL DEFAULT 0 CHECK (segment_no >= 0);
-- round 4, MAJOR-5: plan_id is NOT NULL. There is no legacy pre-launch (no published
-- users, no existing rows), so "nullable for legacy" is dead surface. Every segment line
-- — including the degenerate segment_no=0 single line on the no-change path — carries the
-- plan it was priced under. The reconciler ALWAYS writes it (the N=0 path writes the app's
-- current apps.plan_id at segment_no=0). NOT NULL is added in the SAME statement as the
-- column so the freshly-created, empty invoice_lines accepts it with no backfill.
ALTER TABLE zeroship.invoice_lines
    ADD COLUMN plan_id TEXT NOT NULL REFERENCES zeroship.plans(id) ON DELETE RESTRICT;  -- the segment's plan; always set by the reconciler (NOT NULL — no legacy pre-launch)
ALTER TABLE zeroship.invoice_lines DROP CONSTRAINT invoice_lines_pkey;
ALTER TABLE zeroship.invoice_lines
    ADD CONSTRAINT invoice_lines_pkey PRIMARY KEY (invoice_id, app_id, segment_no);

-- 3. Widen billing_line_provider_refs to carry segment_no and re-add the composite FK to
--    the new 3-col PK, so a per-segment provider ref (each segment is its own Stripe
--    invoice_item) cannot reference a non-existent (invoice, app, segment) line.
ALTER TABLE zeroship.billing_line_provider_refs
    ADD COLUMN segment_no SMALLINT NOT NULL DEFAULT 0 CHECK (segment_no >= 0);
ALTER TABLE zeroship.billing_line_provider_refs DROP CONSTRAINT billing_line_provider_refs_pkey;
ALTER TABLE zeroship.billing_line_provider_refs
    ADD CONSTRAINT billing_line_provider_refs_pkey
    PRIMARY KEY (invoice_id, app_id, segment_no, provider, ref_kind);
ALTER TABLE zeroship.billing_line_provider_refs
    ADD CONSTRAINT billing_line_provider_refs_line_fk
    FOREIGN KEY (invoice_id, app_id, segment_no)
    REFERENCES zeroship.invoice_lines(invoice_id, app_id, segment_no) ON DELETE CASCADE;
--rollback ALTER TABLE zeroship.billing_line_provider_refs DROP CONSTRAINT billing_line_provider_refs_line_fk;
--rollback ALTER TABLE zeroship.billing_line_provider_refs DROP CONSTRAINT billing_line_provider_refs_pkey;
--rollback ALTER TABLE zeroship.billing_line_provider_refs DROP COLUMN segment_no;
--rollback ALTER TABLE zeroship.billing_line_provider_refs ADD CONSTRAINT billing_line_provider_refs_pkey PRIMARY KEY (invoice_id, app_id, provider, ref_kind);
--rollback ALTER TABLE zeroship.invoice_lines DROP CONSTRAINT invoice_lines_pkey;
--rollback ALTER TABLE zeroship.invoice_lines ADD CONSTRAINT invoice_lines_pkey PRIMARY KEY (invoice_id, app_id);
--rollback ALTER TABLE zeroship.invoice_lines DROP COLUMN plan_id;
--rollback ALTER TABLE zeroship.invoice_lines DROP COLUMN segment_no;
--rollback ALTER TABLE zeroship.billing_line_provider_refs ADD CONSTRAINT billing_line_provider_refs_line_fk FOREIGN KEY (invoice_id, app_id) REFERENCES zeroship.invoice_lines(invoice_id, app_id) ON DELETE CASCADE;

--changeset zeroship:invoice-lines-segment-delete-grants splitStatements:false
-- round 4, MAJOR-1: the reconcile now DELETEs orphaned DRAFT segment lines + their
-- provider-refs when a re-drive builds FEWER segments than a prior crashed drive
-- posted (else the draft invoice sweeps the stale higher-segment Stripe items and
-- the finalized subtotal disagrees with the Stripe total — an over-charge). Draft
-- lines are mutable until finalize (the immutability trigger only fires on a
-- finalized parent), but the 0042 grants gave `zeroship_control` only
-- SELECT/INSERT/UPDATE on invoice_lines and SELECT/INSERT on
-- billing_line_provider_refs — no DELETE. Add DELETE so the orphan reconciliation
-- can run as the least-privilege control role (not just as a superuser in tests).
-- The finalized-parent immutability trigger still rejects any DELETE once the
-- invoice is finalized, so this widens privilege only for the still-draft window.
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT DELETE ON zeroship.invoice_lines              TO zeroship_control';
    EXECUTE 'GRANT DELETE ON zeroship.billing_line_provider_refs TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE DELETE ON zeroship.invoice_lines FROM zeroship_control'; EXECUTE 'REVOKE DELETE ON zeroship.billing_line_provider_refs FROM zeroship_control'; END IF; END $rb$;
