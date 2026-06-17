DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.usage_aggregates FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.usage_reports_seen FROM zeroship_control'; END IF; END $rb$;
DROP TABLE zeroship.usage_reports_seen;
DROP POLICY IF EXISTS tenant_isolation ON zeroship.usage_aggregates;
ALTER TABLE zeroship.usage_aggregates NO FORCE ROW LEVEL SECURITY;
ALTER TABLE zeroship.usage_aggregates DISABLE ROW LEVEL SECURITY;
DROP INDEX IF EXISTS zeroship.usage_aggregates_period_idx;
DROP TABLE zeroship.usage_aggregates;
