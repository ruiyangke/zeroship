DO $rb$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_auth') THEN
    EXECUTE 'REVOKE ALL ON zeroship.signing_keys FROM zeroship_auth';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_gateway') THEN
    EXECUTE 'REVOKE ALL ON zeroship.signing_keys FROM zeroship_gateway';
  END IF;
END $rb$;

DROP INDEX IF EXISTS zeroship.signing_keys_status_idx;
DROP TABLE zeroship.signing_keys;
