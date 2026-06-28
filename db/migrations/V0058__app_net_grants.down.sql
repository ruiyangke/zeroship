DO $rb$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'REVOKE ALL ON zeroship.app_net_grants FROM zeroship_control';
    EXECUTE 'REVOKE ALL ON zeroship.net_policy_catalog FROM zeroship_control';
  END IF;
END $rb$;

DELETE FROM zeroship.metric_weights WHERE metric = 'net_ingress_bytes';
DELETE FROM zeroship.billing_metrics WHERE metric = 'net_ingress_bytes';

ALTER TABLE zeroship.plans DROP COLUMN net_policy_limits_json;
DROP TABLE zeroship.net_policy_catalog;
DROP INDEX IF EXISTS zeroship.app_net_grants_app_id_idx;
DROP TABLE zeroship.app_net_grants;
