DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.metric_weights FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.pricing_config FROM zeroship_control'; END IF; END $rb$;
SELECT 1;
DELETE FROM zeroship.pricing_config WHERE id = 'global';
DELETE FROM zeroship.metric_weights;
DROP TABLE zeroship.pricing_config;
DROP TABLE zeroship.metric_weights;
