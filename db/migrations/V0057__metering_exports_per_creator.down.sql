DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metering_exports FROM zeroship_control'; END IF; END $rb$;
DROP TABLE zeroship.metering_exports;
DROP TABLE IF EXISTS zeroship.metering_exports;
