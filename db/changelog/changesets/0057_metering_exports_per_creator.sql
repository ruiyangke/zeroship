--liquibase formatted sql

-- PER-CREATOR metering-export high-water (HIGH-severity revenue-loss fix).
--
-- The 0043 `metering_exports` table keyed the export high-water on (app_id, period)
-- and the cron pushed a PER-APP billable delta. But the external meter rails it
-- feeds — Stripe Billing Meters (`event_summaries?customer=…`) and OpenMeter
-- (`subject = creator handle`) — aggregate PER CUSTOMER, not per app. So the C2
-- re-drive reconcile (`already = max(high_water, external_aggregate)`) compared a
-- per-app delta against a per-CUSTOMER aggregate: once the creator's FIRST app
-- pushed its CU, the customer aggregate covered it, and EVERY subsequent app of the
-- same creator computed delta = 0 — its CU never billed, and its high-water silently
-- advanced to mask the gap. Every metered app after the first under-billed.
--
-- The fix reconciles + pushes at the grain the external meter actually aggregates:
-- PER CREATOR / per customer. The high-water is therefore re-keyed on
-- (creator_id, period): the cron sums BILLABLE CU across ALL the creator's apps,
-- subtracts the creator-level included quota ONCE, and pushes a single customer-level
-- delta reconciled against the customer aggregate.
--
-- Pre-launch, no back-compat (AGENTS.md): the 0043 (app_id, period) shape is dropped
-- and replaced wholesale — there are no production rows to migrate.

--changeset zeroship:metering-exports-drop-per-app splitStatements:true
-- Drop the per-app table + its RLS policy (0043). The replacement is creator-keyed
-- control bookkeeping (like `invoices` / `creator_billing`), so it carries NO app
-- RLS. NOTE: there is intentionally no rollback to the OLD per-app shape — that shape
-- is the bug; reverting would re-open the revenue-loss class.
DROP TABLE IF EXISTS zeroship.metering_exports;
--rollback DROP TABLE IF EXISTS zeroship.metering_exports;

--changeset zeroship:metering-exports-per-creator splitStatements:true
-- Export-only; NEVER feeds enforcement (the spend cap reads usage_aggregates directly,
-- keeping providers pluggable). Creator(customer)-keyed: one high-water per
-- (creator, period), matching the per-customer grain the Stripe/OpenMeter meter
-- aggregates at. `exported_units` is the cumulative BILLABLE CU summed across ALL the
-- creator's apps (gross_creator − included_creator), monotonic. The M2 durable
-- failure surface (consecutive_failures / last_error / last_attempt_at) is kept
-- verbatim, now per creator. creator_id FK → creator_billing (the same parent
-- `invoices` uses) ON DELETE CASCADE: a creator with a saved customer always has a
-- creator_billing row (stripe_store::set_customer creates it first).
CREATE TABLE zeroship.metering_exports (
    creator_id           UUID                    NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    period               zeroship.billing_period NOT NULL,
    exported_units       BIGINT                  NOT NULL DEFAULT 0 CHECK (exported_units >= 0),  -- cumulative BILLABLE CU across the creator's apps (monotonic)
    consecutive_failures INTEGER                 NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    last_error           TEXT,                                                                    -- redacted at source
    last_attempt_at      TIMESTAMPTZ,
    updated_at           TIMESTAMPTZ             NOT NULL DEFAULT NOW(),
    PRIMARY KEY (creator_id, period)
);
--rollback DROP TABLE zeroship.metering_exports;

--changeset zeroship:metering-exports-per-creator-grants splitStatements:false rollbackSplitStatements:false
-- Creator(user)-keyed control bookkeeping ⇒ no app RLS (uniform with creator_billing
-- / invoices, which are likewise creator-keyed and RLS-free: control connects
-- BYPASSRLS and is the only writer). Least-priv grant guarded on role existence.
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.metering_exports TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metering_exports FROM zeroship_control'; END IF; END $rb$;
