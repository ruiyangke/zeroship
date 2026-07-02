-- Platform OP signing-key registry.
--
-- This table is public/metadata only: the active Ed25519 private key lives in
-- AUTH_SIGNING_KEY_FILE (or an equivalent KMS custody path) and is never stored
-- in Postgres. Rows here are the JWKS/rotation registry the auth service writes
-- and the gateway reads for local platform-token verification.

CREATE TABLE zeroship.signing_keys (
    kid          TEXT        PRIMARY KEY,
    alg          TEXT        NOT NULL CHECK (alg = 'EdDSA'),
    public_jwk   JSONB       NOT NULL,
    status       TEXT        NOT NULL CHECK (status IN ('active', 'next', 'retiring')),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    activated_at TIMESTAMPTZ,
    retiring_at  TIMESTAMPTZ,
    retired_at   TIMESTAMPTZ
);

CREATE INDEX signing_keys_status_idx
    ON zeroship.signing_keys (status);

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_auth') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.signing_keys TO zeroship_auth';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_gateway') THEN
    EXECUTE 'GRANT SELECT ON zeroship.signing_keys TO zeroship_gateway';
  END IF;
END $g$;
