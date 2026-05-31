--liquibase formatted sql

-- auth.* schema. Transcribed verbatim from crates/auth/src/store/migrations.rs
-- (the STATEMENTS array). Every object reference is fully schema-qualified.
-- zeroship.users is created first; all other auth tables FK into it.

--changeset zeroship:auth-users splitStatements:true
CREATE TABLE zeroship.users (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email             CITEXT UNIQUE NOT NULL,
    email_verified_at TIMESTAMPTZ,
    name              TEXT NOT NULL,
    avatar_url        TEXT,
    password_hash     TEXT,
    credential_version BIGINT NOT NULL DEFAULT 0,
    locked_until      TIMESTAMPTZ,
    -- Soft-disable flag (NULL = active). Read by the login/link eligibility
    -- checks (ui/login.rs, ui/link.rs, identity/eligibility.rs) and returned by
    -- store::users; the account is rejected while non-NULL.
    disabled_at       TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login_at     TIMESTAMPTZ
);
--rollback DROP TABLE zeroship.users;

--changeset zeroship:auth-identities splitStatements:true
CREATE TABLE zeroship.identities (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    provider      TEXT NOT NULL,
    subject       TEXT NOT NULL,
    email_at_link CITEXT,
    raw_profile   JSONB,
    linked_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (provider, subject)
);
--rollback DROP TABLE zeroship.identities;

--changeset zeroship:auth-sessions splitStatements:true
CREATE TABLE zeroship.sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES zeroship.users(id),
    auth_method     TEXT NOT NULL,
    amr             TEXT[] NOT NULL,
    acr             TEXT,
    auth_time       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    credential_version BIGINT NOT NULL DEFAULT 0,
    idle_expires_at TIMESTAMPTZ NOT NULL,
    abs_expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at      TIMESTAMPTZ
);
--rollback DROP TABLE zeroship.sessions;

--changeset zeroship:auth-magic-links splitStatements:true
CREATE TABLE zeroship.magic_links (
    token_hash  BYTEA PRIMARY KEY,
    email       CITEXT NOT NULL,
    csrf_nonce  TEXT NOT NULL,
    purpose     TEXT NOT NULL,
    request_ip  INET,
    request_ua  TEXT,
    issued_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at  TIMESTAMPTZ NOT NULL,
    consumed_pending_at TIMESTAMPTZ,
    consumed_at TIMESTAMPTZ
);
CREATE INDEX auth_magic_email_idx ON zeroship.magic_links (email);
--rollback DROP TABLE zeroship.magic_links;

--changeset zeroship:auth-magic-completions splitStatements:true
CREATE TABLE zeroship.magic_completions (
    csrf_nonce       TEXT PRIMARY KEY,
    code             TEXT NOT NULL,
    email            CITEXT NOT NULL,
    login_challenge  TEXT NOT NULL,
    attempts         SMALLINT NOT NULL DEFAULT 0,
    expires_at       TIMESTAMPTZ NOT NULL,
    consumed_pending_at TIMESTAMPTZ,
    consumed_at      TIMESTAMPTZ
);
CREATE INDEX auth_magic_completions_expires_idx
    ON zeroship.magic_completions (expires_at) WHERE consumed_at IS NULL;
--rollback DROP TABLE zeroship.magic_completions;

--changeset zeroship:auth-email-verifications splitStatements:true
CREATE TABLE zeroship.email_verifications (
    token_hash  BYTEA PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    email       CITEXT NOT NULL,
    issued_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at  TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ
);
--rollback DROP TABLE zeroship.email_verifications;

--changeset zeroship:auth-email-suppressions splitStatements:true
CREATE TABLE zeroship.email_suppressions (
    email         CITEXT PRIMARY KEY,
    reason        TEXT NOT NULL,
    suppressed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    provider_msg  TEXT
);
--rollback DROP TABLE zeroship.email_suppressions;

--changeset zeroship:auth-rate-limits splitStatements:true
CREATE TABLE zeroship.rate_limits (
    bucket_key TEXT PRIMARY KEY,
    tokens     REAL NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
--rollback DROP TABLE zeroship.rate_limits;

--changeset zeroship:auth-audit-events splitStatements:true
CREATE TABLE zeroship.audit_events (
    id          BIGSERIAL PRIMARY KEY,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    event_type  TEXT NOT NULL,
    outcome     TEXT NOT NULL,
    user_id     UUID,
    client_id   TEXT,
    request_id  TEXT,
    ip          INET,
    user_agent  TEXT,
    auth_method TEXT,
    detail      JSONB
);
CREATE INDEX auth_audit_user_idx  ON zeroship.audit_events (user_id, occurred_at);
CREATE INDEX auth_audit_event_idx ON zeroship.audit_events (event_type, occurred_at);
--rollback DROP TABLE zeroship.audit_events;

-- Append-only guard for zeroship.audit_events: a trigger function that raises on
-- any UPDATE/DELETE/TRUNCATE, the three triggers wiring it, the PUBLIC revoke,
-- and the per-role revoke loop (guards on role existence). splitStatements:false
-- because the function body and the DO block contain `;` inside `$$`.
--changeset zeroship:auth-audit-events-guard splitStatements:false
CREATE OR REPLACE FUNCTION zeroship.audit_events_block_tamper()
 RETURNS trigger AS $$
 BEGIN
     -- Append-only: UPDATE and TRUNCATE are always rejected. The one sanctioned
     -- DELETE is the retention sweep (auth::cron::audit_retention), which flags
     -- its connection with `SET zeroship.audit_retention = 'on'` before deleting
     -- expired rows. App handlers never set that GUC (and can't via a
     -- parameterised query), so the tamper guard still holds against application
     -- code and SQL injection.
     IF TG_OP = 'DELETE'
        AND current_setting('zeroship.audit_retention', true) = 'on' THEN
         RETURN OLD;
     END IF;
     RAISE EXCEPTION 'zeroship.audit_events is append-only'
         USING ERRCODE = 'insufficient_privilege';
 END
 $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS audit_events_block_update ON zeroship.audit_events;
CREATE TRIGGER audit_events_block_update
    BEFORE UPDATE ON zeroship.audit_events
    FOR EACH ROW EXECUTE FUNCTION zeroship.audit_events_block_tamper();
DROP TRIGGER IF EXISTS audit_events_block_delete ON zeroship.audit_events;
CREATE TRIGGER audit_events_block_delete
    BEFORE DELETE ON zeroship.audit_events
    FOR EACH ROW EXECUTE FUNCTION zeroship.audit_events_block_tamper();
DROP TRIGGER IF EXISTS audit_events_block_truncate ON zeroship.audit_events;
CREATE TRIGGER audit_events_block_truncate
    BEFORE TRUNCATE ON zeroship.audit_events
    FOR EACH STATEMENT EXECUTE FUNCTION zeroship.audit_events_block_tamper();
REVOKE UPDATE, DELETE, TRUNCATE ON TABLE zeroship.audit_events FROM PUBLIC;
DO $$
 DECLARE
     role_name TEXT;
 BEGIN
     FOREACH role_name IN ARRAY ARRAY[
         'zeroship_auth',
         'zeroship_control',
         'zeroship_gateway',
         'zeroship_worker',
         'zeroship_app',
         'auth',
         'control'
     ] LOOP
         IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = role_name) THEN
             EXECUTE format(
                 'REVOKE UPDATE, DELETE, TRUNCATE ON TABLE zeroship.audit_events FROM %I',
                 role_name
             );
         END IF;
     END LOOP;
 END
 $$;
--rollback DROP TRIGGER IF EXISTS audit_events_block_truncate ON zeroship.audit_events;
--rollback DROP TRIGGER IF EXISTS audit_events_block_delete ON zeroship.audit_events;
--rollback DROP TRIGGER IF EXISTS audit_events_block_update ON zeroship.audit_events;
--rollback DROP FUNCTION IF EXISTS zeroship.audit_events_block_tamper();

--changeset zeroship:auth-gateway-sessions splitStatements:true
CREATE TABLE zeroship.gateway_sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The gateway session store (gateway::sessions) parses the subject into a
    -- Uuid and binds/reads it as UUID, so the column is UUID (not TEXT).
    user_id         UUID NOT NULL,
    app_id          TEXT NOT NULL,
    email           CITEXT,
    name            TEXT,
    avatar_url      TEXT,
    email_verified  BOOLEAN NOT NULL DEFAULT FALSE,
    issued_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    idle_expires_at TIMESTAMPTZ NOT NULL,
    abs_expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at      TIMESTAMPTZ
);
CREATE INDEX auth_gateway_sessions_app_idx ON zeroship.gateway_sessions (app_id, user_id);
CREATE INDEX auth_gateway_sessions_idle_idx ON zeroship.gateway_sessions (idle_expires_at) WHERE revoked_at IS NULL;
--rollback DROP TABLE zeroship.gateway_sessions;

--changeset zeroship:auth-dpop-jti splitStatements:true
CREATE TABLE zeroship.dpop_jti (
    jti         TEXT PRIMARY KEY,
    inserted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX auth_dpop_jti_inserted_idx
    ON zeroship.dpop_jti (inserted_at);
--rollback DROP TABLE zeroship.dpop_jti;

--changeset zeroship:auth-console-sessions splitStatements:true
CREATE TABLE zeroship.console_sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The console-session store (control::console_sessions) parses the OIDC
    -- subject into a Uuid and binds/reads it as UUID, so the column is UUID.
    user_id         UUID NOT NULL,
    email           CITEXT,
    name            TEXT,
    avatar_url      TEXT,
    email_verified  BOOLEAN NOT NULL DEFAULT FALSE,
    issued_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    idle_expires_at TIMESTAMPTZ NOT NULL,
    abs_expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at      TIMESTAMPTZ
);
CREATE INDEX auth_console_sessions_user_idx ON zeroship.console_sessions (user_id);
CREATE INDEX auth_console_sessions_idle_idx ON zeroship.console_sessions (idle_expires_at) WHERE revoked_at IS NULL;
--rollback DROP TABLE zeroship.console_sessions;

--changeset zeroship:auth-cron-state splitStatements:true
CREATE TABLE zeroship.cron_state (
    key             TEXT PRIMARY KEY,
    last_rotated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    notes           TEXT
);
--rollback DROP TABLE zeroship.cron_state;

-- Missing tables (cron bug fix). The jwk-rotation cron task references this
-- but no migration ever created it. Inferred from
-- crates/auth/src/cron/jwk_rotation.rs.
--
-- (The former zeroship.wrapper_revoked_subjects global subject denylist was
-- removed in Batch A M2 — it was write-only dead code after the per-app
-- pws_ revocation cutover; the per-app zeroship.token_revocations family marker
-- below is the sole wrapper-token revocation primitive.)
--changeset zeroship:auth-jwk-key-state splitStatements:true
CREATE TABLE zeroship.jwk_key_state (
    set_name   TEXT NOT NULL,
    kid        TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (set_name, kid)
);
--rollback DROP TABLE zeroship.jwk_key_state;

-- Cross-node access-token revocation family marker (spec §8.5). The
-- PRIMARY revocation mechanism for the Bearer arm: keyed PER-APP on
-- (client_id, sub) so revoking a user on app A leaves app B untouched.
-- `sub` is TEXT to hold BOTH the wrapper's pws_ pairwise subject and the
-- raw-Hydra global UUID. The Bearer arm rejects a token when a row exists
-- with revoked_after > token.iat. See crates/core/src/wrapper_revocation.rs
-- (revoke_family / is_family_revoked_since / sweep_expired_families).
--changeset zeroship:auth-token-revocations splitStatements:true
CREATE TABLE zeroship.token_revocations (
    client_id     TEXT        NOT NULL,
    sub           TEXT        NOT NULL,
    revoked_after TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (client_id, sub)
);
--rollback DROP TABLE zeroship.token_revocations;
