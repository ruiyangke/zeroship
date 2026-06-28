-- TOTP two-factor authentication (ISS-11).
--
-- Two tables, both owned by the auth service and scoped per-user:
--
--   zeroship.totp_credentials  — at most one row per user (PK = user_id). The
--     RFC 6238 shared secret is stored ENCRYPTED AT REST (AES-256-GCM via
--     zeroship_core::crypto, keyed by AUTH_TOTP_ENC_KEY, AAD-bound to the
--     user_id). `confirmed_at` is NULL while enrollment is pending — the row
--     exists (a secret was generated and the provisioning URI handed out) but
--     2FA is NOT yet enforced. It flips to NOW() on the first valid code, which
--     is the moment the login challenge starts gating that user's password
--     login. A re-enroll overwrites the pending row (INSERT ... ON CONFLICT).
--
--   zeroship.totp_backup_codes — one row per single-use recovery code, minted
--     at confirm time. `code_hash` is an Argon2id PHC string (the SAME hashing
--     the password column uses — never store the plaintext code). `used_at`
--     stamps the moment a code is redeemed; a redeemed code is rejected forever
--     after (single-use). Codes are deleted with the credential on disable.
--
-- Both FK user_id → zeroship.users ON DELETE CASCADE so the GDPR-erase reaper
-- (ISS-12) tears down a user's 2FA state along with the rest of their cascade
-- dependents. No backfill (pre-launch, no rows).

CREATE TABLE zeroship.totp_credentials (
    user_id          UUID PRIMARY KEY REFERENCES zeroship.users (id) ON DELETE CASCADE,
    encrypted_secret BYTEA       NOT NULL,
    confirmed_at     TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE zeroship.totp_backup_codes (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id    UUID        NOT NULL REFERENCES zeroship.users (id) ON DELETE CASCADE,
    code_hash  TEXT        NOT NULL,
    used_at    TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The backup-code redeem path scans a user's unused codes; index user_id so the
-- scan stays per-user (a user holds only a handful of codes, but the index keeps
-- the verify-time lookup off a seq scan as the table grows across all users).
CREATE INDEX auth_totp_backup_codes_user_idx
    ON zeroship.totp_backup_codes (user_id);

-- Least-privilege grants for the auth role (mirrors the 0025 GRANT MATRIX style;
-- new tables added post-0025 carry their grants in the table's own changeset):
--   totp_credentials  — enroll (INSERT/UPSERT), confirm (UPDATE), read at login
--                        (SELECT), disable (DELETE).
--   totp_backup_codes — mint (INSERT), read+mark-used at redeem (SELECT/UPDATE),
--                        disable (DELETE).
GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.totp_credentials  TO zeroship_auth;
GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.totp_backup_codes TO zeroship_auth;
