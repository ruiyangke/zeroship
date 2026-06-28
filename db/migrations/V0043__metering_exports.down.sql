DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metering_exports FROM zeroship_control'; END IF; END $rb$;
DROP POLICY IF EXISTS tenant_isolation ON zeroship.metering_exports;
ALTER TABLE zeroship.metering_exports NO FORCE ROW LEVEL SECURITY;
ALTER TABLE zeroship.metering_exports DISABLE ROW LEVEL SECURITY;
DROP TABLE zeroship.metering_exports;
