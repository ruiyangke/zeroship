DROP INDEX IF EXISTS zeroship.auth_gateway_sessions_app_sid_idx;
ALTER TABLE zeroship.gateway_sessions
    DROP COLUMN IF EXISTS sid;

DROP TABLE IF EXISTS zeroship.oidc_session_clients;

ALTER TABLE zeroship.oauth_authorization_codes
    DROP COLUMN IF EXISTS sid;

ALTER TABLE zeroship.oauth_clients
    DROP COLUMN IF EXISTS backchannel_logout_uri;
