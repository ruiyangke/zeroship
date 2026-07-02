-- Platform-mediated OAuth device flow for platform deploy tokens.
--
-- Control owns the pending grant and polling discipline; the platform OP
-- (auth service) owns access-token issuance. The poll secret (`device_code`) is
-- never stored in plaintext: callers look up rows by SHA-256 hash, while the
-- low-entropy `user_code` is only the human-facing selector for an
-- already-authenticated approval.

CREATE TABLE zeroship.device_grants (
    device_code_hash         TEXT        PRIMARY KEY,
    user_code                TEXT        NOT NULL UNIQUE,
    status                   TEXT        NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'denied')),
    principal_id             UUID        REFERENCES zeroship.users(id),
    platform_access_token_enc BYTEA,
    provider                 TEXT        NOT NULL,
    scope                    TEXT,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at               TIMESTAMPTZ NOT NULL,
    last_polled_at           TIMESTAMPTZ
);

CREATE INDEX device_grants_expires_at_idx
    ON zeroship.device_grants (expires_at);

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.device_grants TO zeroship_control';
  END IF;
END $g$;
