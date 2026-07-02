DO $rb$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'REVOKE ALL ON zeroship.device_grants FROM zeroship_control';
  END IF;
END $rb$;

DROP INDEX IF EXISTS zeroship.device_grants_expires_at_idx;
DROP TABLE zeroship.device_grants;
