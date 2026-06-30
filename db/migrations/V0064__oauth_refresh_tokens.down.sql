DO $rb$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_auth') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_refresh_tokens FROM zeroship_auth';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_refresh_tokens FROM zeroship_control';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_gateway') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_refresh_tokens FROM zeroship_gateway';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_worker') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_refresh_tokens FROM zeroship_worker';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_app') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oauth_refresh_tokens FROM zeroship_app';
  END IF;
END
$rb$;

DROP INDEX IF EXISTS zeroship.oauth_refresh_tokens_idem_reap_idx;
DROP INDEX IF EXISTS zeroship.oauth_refresh_tokens_expires_at_idx;
DROP INDEX IF EXISTS zeroship.oauth_refresh_tokens_user_idx;
DROP INDEX IF EXISTS zeroship.oauth_refresh_tokens_family_idx;
DROP INDEX IF EXISTS zeroship.oauth_refresh_tokens_one_active_per_family;
DROP TABLE IF EXISTS zeroship.oauth_refresh_tokens;

ALTER TABLE zeroship.device_grants
    DROP COLUMN IF EXISTS auth_credential_version;

ALTER TABLE zeroship.oauth_authorization_codes
    DROP COLUMN IF EXISTS auth_credential_version;

ALTER TABLE zeroship.oauth_clients
    DROP COLUMN IF EXISTS token_endpoint_auth_method,
    DROP COLUMN IF EXISTS refresh_allowed,
    DROP COLUMN IF EXISTS client_secret_hash;
