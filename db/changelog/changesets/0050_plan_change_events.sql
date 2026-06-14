--liquibase formatted sql

--changeset zeroship:plan-change-events splitStatements:true
-- PRORATION (gap #26 D) — FULL USAGE-SEGMENT (round 3, user decision 1 OVERRIDE).
-- Recorded on EVERY set_plan (always, cheap, append-only). App-keyed (the plan is
-- per-app) ⇒ FORCE RLS (uniform with app_spend_*). A mid-period plan change creates
-- ONE row; an app with N change rows in a period splits into N+1 SEGMENTS at month-end,
-- and EACH segment is priced under its own plan and frozen as a SEPARATE invoice line.
--
-- THE HARD PROBLEM this row solves. usage_aggregates is keyed (app_id, period, metric)
-- with a SINGLE running cumulative `total` for the WHOLE calendar month — there is NO
-- per-event timestamp and NO sub-period bucket. So you CANNOT split a period's usage
-- across a pre-change and a post-change plan segment by time-filtering events: the
-- events are gone, only the running total remains.
--
-- THE SOLUTION. Because the counters are MONOTONIC cumulative totals, we SNAPSHOT the
-- app's cumulative usage_aggregates totals (per metric) ONTO this row AT THE CHANGE
-- INSTANT — `usage_at_change` JSONB {metric: cumulative_total_at_change}, read SERVER-
-- SIDE in the SAME txn as the plan flip. At month-end a segment's usage-delta per metric
-- is `(cumulative at segment END) − (cumulative at segment START)`; the delta is floored
-- at max(0, …) so a vanished metric never credits the bill (MAJOR-3). Server-derived
-- frozen money fields (from/to_base_fee_cents from the catalog) keep proration
-- reproducible without reading the live plans table (CRITICAL-4: NEVER client-supplied).
CREATE TABLE zeroship.plan_change_events (
    id              TEXT        PRIMARY KEY,            -- pce_<base62>
    app_id          UUID        NOT NULL REFERENCES zeroship.apps(id)  ON DELETE CASCADE,
    period          zeroship.billing_period NOT NULL,  -- first-of-month the change falls in
    from_plan_id    TEXT        REFERENCES zeroship.plans(id) ON DELETE RESTRICT,  -- NULL = initial assignment
    to_plan_id      TEXT        NOT NULL REFERENCES zeroship.plans(id) ON DELETE RESTRICT,
    -- The instant the change took effect, for the base-fee day-fraction. Frozen at
    -- write so a later clock read can't move it.
    effective_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Frozen base-fee snapshot of BOTH plans at change time (snapshot-onto-line:
    -- proration stays reproducible without reading the live plans table). NULL from-fee
    -- for the initial assignment.
    from_base_fee_cents BIGINT  CHECK (from_base_fee_cents IS NULL OR from_base_fee_cents >= 0),
    to_base_fee_cents   BIGINT  NOT NULL CHECK (to_base_fee_cents >= 0),
    -- THE CUMULATIVE USAGE CHECKPOINT (round 3). {metric: cumulative_total} read from
    -- usage_aggregates in the SAME txn as the plan flip. This is the segment boundary
    -- marker: the END of the segment that just closed and the START of the one opening.
    -- Server-derived, never client-supplied. A metric absent from the JSON is treated as
    -- cumulative 0 at this instant (it had no usage yet). For the initial assignment
    -- (from_plan_id IS NULL) this is typically {} (no usage before the app existed).
    usage_at_change JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX plan_change_events_app_period_idx
    ON zeroship.plan_change_events (app_id, period);
--rollback DROP INDEX IF EXISTS zeroship.plan_change_events_app_period_idx;
--rollback DROP TABLE zeroship.plan_change_events;

--changeset zeroship:plan-change-events-rls splitStatements:true
ALTER TABLE zeroship.plan_change_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE zeroship.plan_change_events FORCE  ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON zeroship.plan_change_events
    USING (app_id = current_setting('zeroship.tenant_app', true)::uuid);
--rollback DROP POLICY IF EXISTS tenant_isolation ON zeroship.plan_change_events;
--rollback ALTER TABLE zeroship.plan_change_events NO FORCE ROW LEVEL SECURITY;
--rollback ALTER TABLE zeroship.plan_change_events DISABLE ROW LEVEL SECURITY;

--changeset zeroship:plan-change-events-immutable splitStatements:false
-- Append-only: a plan-change event, once written, is an immutable timeline fact
-- (uniform with the credit_ledger / invoice / line immutability discipline). The
-- segment split + base-fee proration replay bit-for-bit off these frozen rows, so a
-- later edit/delete would silently re-price a closed period. Reject UPDATE and DELETE.
CREATE FUNCTION zeroship.plan_change_events_immutable() RETURNS trigger AS $fn$
BEGIN
    RAISE EXCEPTION 'plan_change_events is append-only (no UPDATE/DELETE) — the proration timeline is frozen';
END;
$fn$ LANGUAGE plpgsql;
CREATE TRIGGER plan_change_events_immutable_trg
    BEFORE UPDATE OR DELETE ON zeroship.plan_change_events
    FOR EACH ROW EXECUTE FUNCTION zeroship.plan_change_events_immutable();
--rollback DROP TRIGGER IF EXISTS plan_change_events_immutable_trg ON zeroship.plan_change_events;
--rollback DROP FUNCTION IF EXISTS zeroship.plan_change_events_immutable();

--changeset zeroship:plan-change-events-grants splitStatements:false
DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- Append-only: SELECT/INSERT, no UPDATE/DELETE.
    EXECUTE 'GRANT SELECT, INSERT ON zeroship.plan_change_events TO zeroship_control';
  END IF;
END $g$;
--rollback DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.plan_change_events FROM zeroship_control'; END IF; END $rb$;
