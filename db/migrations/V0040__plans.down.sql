DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.plans FROM zeroship_control'; END IF; END $rb$;
DROP INDEX IF EXISTS zeroship.apps_plan_id_idx;
ALTER TABLE zeroship.apps DROP CONSTRAINT apps_plan_fk;
DROP TABLE zeroship.plans;
