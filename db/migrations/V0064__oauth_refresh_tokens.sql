-- Platform OP refresh-token families (P5b).
--
-- Refresh tokens are CLI / programmatic only. The raw token is never stored:
-- token_hash is HMAC-SHA256(refresh_hash_key[hash_key_version], raw). The only
-- raw successor material at rest is the bounded-idempotency response, sealed by
-- the auth service and fenced to zeroship_auth.

ALTER TABLE zeroship.oauth_clients
    ADD COLUMN client_secret_hash TEXT,
    ADD COLUMN refresh_allowed BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN token_endpoint_auth_method TEXT NOT NULL DEFAULT 'none'
        CHECK (token_endpoint_auth_method IN ('none', 'client_secret_basic', 'client_secret_post'));

ALTER TABLE zeroship.oauth_authorization_codes
    ADD COLUMN auth_credential_version BIGINT NOT NULL DEFAULT 0;

ALTER TABLE zeroship.device_grants
    ADD COLUMN auth_credential_version BIGINT NOT NULL DEFAULT 0;

CREATE TABLE zeroship.oauth_refresh_tokens (
    token_hash                  BYTEA       PRIMARY KEY,
    hash_key_version            SMALLINT    NOT NULL,
    refresh_family_id           TEXT        NOT NULL,
    replaced_by_token_hash      BYTEA,
    client_id                   TEXT        NOT NULL
        REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE,
    user_id                     UUID        NOT NULL
        REFERENCES zeroship.users(id) ON DELETE CASCADE,
    sub                         TEXT        NOT NULL,
    granted_scopes              TEXT[]      NOT NULL,
    family_granted_scopes       TEXT[]      NOT NULL,
    issued_at                   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at                  TIMESTAMPTZ NOT NULL,
    family_absolute_expires_at  TIMESTAMPTZ NOT NULL,
    consumed_at                 TIMESTAMPTZ,
    rotated_at                  TIMESTAMPTZ,
    revoked_at                  TIMESTAMPTZ,
    last_used_at                TIMESTAMPTZ,
    idem_response_enc           BYTEA,
    idem_expires_at             TIMESTAMPTZ,
    CONSTRAINT oauth_refresh_tokens_idle_le_ceiling
        CHECK (expires_at <= family_absolute_expires_at)
);

CREATE UNIQUE INDEX oauth_refresh_tokens_one_active_per_family
    ON zeroship.oauth_refresh_tokens (refresh_family_id)
    WHERE rotated_at IS NULL AND revoked_at IS NULL;

CREATE INDEX oauth_refresh_tokens_family_idx
    ON zeroship.oauth_refresh_tokens (refresh_family_id, client_id, user_id);

CREATE INDEX oauth_refresh_tokens_user_idx
    ON zeroship.oauth_refresh_tokens (user_id);

CREATE INDEX oauth_refresh_tokens_expires_at_idx
    ON zeroship.oauth_refresh_tokens (expires_at);

CREATE INDEX oauth_refresh_tokens_idem_reap_idx
    ON zeroship.oauth_refresh_tokens (idem_expires_at)
    WHERE idem_response_enc IS NOT NULL;

DO $g$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_auth') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.oauth_refresh_tokens TO zeroship_auth';
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
$g$;
