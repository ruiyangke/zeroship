-- OIDC Back-Channel Logout 1.0 session correlation.
--
-- The OP uses zeroship.idp_sessions.id as the opaque `sid` value it places in
-- ID tokens and logout tokens. The extra columns/tables below persist the
-- session-client relationship needed to emit logout tokens when that OP session
-- ends, and let the gateway RP correlate an inbound `logout_token.sid` back to
-- its local app sessions.

ALTER TABLE zeroship.oauth_clients
    ADD COLUMN backchannel_logout_uri TEXT;

-- Authorization codes are one-use, short-lived prelaunch rows. Existing rows
-- cannot be safely correlated to the OP session that minted them, so remove
-- stale codes before enforcing sid on all newly issued codes.
DELETE FROM zeroship.oauth_authorization_codes;

ALTER TABLE zeroship.oauth_authorization_codes
    ADD COLUMN sid TEXT NOT NULL;

CREATE TABLE zeroship.oidc_session_clients (
    idp_session_id UUID NOT NULL
        REFERENCES zeroship.idp_sessions(id) ON DELETE CASCADE,
    user_id UUID NOT NULL
        REFERENCES zeroship.users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL
        REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE,
    sid TEXT NOT NULL,
    sub TEXT NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (idp_session_id, client_id)
);
CREATE INDEX oidc_session_clients_user_idx
    ON zeroship.oidc_session_clients (user_id, idp_session_id);
CREATE INDEX oidc_session_clients_client_idx
    ON zeroship.oidc_session_clients (client_id);

ALTER TABLE zeroship.gateway_sessions
    ADD COLUMN sid TEXT;
CREATE INDEX auth_gateway_sessions_app_sid_idx
    ON zeroship.gateway_sessions (app_id, sid)
    WHERE sid IS NOT NULL AND revoked_at IS NULL;

DO $g$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_auth') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.oidc_session_clients TO zeroship_auth';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oidc_session_clients FROM zeroship_control';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_gateway') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oidc_session_clients FROM zeroship_gateway';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_worker') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oidc_session_clients FROM zeroship_worker';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_app') THEN
    EXECUTE 'REVOKE ALL ON zeroship.oidc_session_clients FROM zeroship_app';
  END IF;
END
$g$;
