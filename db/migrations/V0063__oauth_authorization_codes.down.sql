DO $rb$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_auth') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_authorization_codes FROM zeroship_auth';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_authorization_codes FROM zeroship_control';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_gateway') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_authorization_codes FROM zeroship_gateway';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_worker') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_authorization_codes FROM zeroship_worker';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_app') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_authorization_codes FROM zeroship_app';
  END IF;
END $rb$;

DROP INDEX IF EXISTS zeroship.oauth_authorization_codes_expires_at_idx;
DROP TABLE zeroship.oauth_authorization_codes;
