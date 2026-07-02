-- Platform OP OAuth authorization-code store.
--
-- Codes are bearer credentials until redeemed, so the raw code is never stored.
-- The row binds the code to the client, exact redirect URI, PKCE S256
-- challenge, granted scopes, nonce, and authenticated user. `/token` consumes
-- rows with one atomic conditional UPDATE and rejects expired/reused codes.

CREATE TABLE zeroship.oauth_authorization_codes (
    code_hash        BYTEA       PRIMARY KEY,
    client_id        TEXT        NOT NULL REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE,
    redirect_uri     TEXT        NOT NULL,
    pkce_challenge   TEXT        NOT NULL,
    pkce_method      TEXT        NOT NULL CHECK (pkce_method = 'S256'),
    requested_scopes TEXT[]      NOT NULL,
    granted_scopes   TEXT[]      NOT NULL,
    nonce            TEXT,
    user_id          UUID        NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at       TIMESTAMPTZ NOT NULL,
    consumed_at      TIMESTAMPTZ,
    CONSTRAINT oauth_authorization_codes_max_ttl
        CHECK (expires_at <= created_at + INTERVAL '60 seconds')
);

CREATE INDEX oauth_authorization_codes_expires_at_idx
    ON zeroship.oauth_authorization_codes (expires_at);

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_auth') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.oauth_authorization_codes TO zeroship_auth';
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
END $g$;
