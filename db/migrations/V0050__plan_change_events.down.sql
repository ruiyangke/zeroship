DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.plan_change_events FROM zeroship_control'; END IF; END $rb$;
DROP TRIGGER IF EXISTS plan_change_events_immutable_trg ON zeroship.plan_change_events;
DROP FUNCTION IF EXISTS zeroship.plan_change_events_immutable();
DROP POLICY IF EXISTS tenant_isolation ON zeroship.plan_change_events;
ALTER TABLE zeroship.plan_change_events NO FORCE ROW LEVEL SECURITY;
ALTER TABLE zeroship.plan_change_events DISABLE ROW LEVEL SECURITY;
DROP INDEX IF EXISTS zeroship.plan_change_events_app_period_idx;
DROP TABLE zeroship.plan_change_events;
