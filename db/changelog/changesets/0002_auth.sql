--liquibase formatted sql

-- auth.* schema. Transcribed verbatim from crates/auth/src/store/migrations.rs
-- (the STATEMENTS array). Every object reference is fully schema-qualified.
-- auth.users is created first; all other auth tables FK into it.

--changeset zeroship:auth-users splitStatements:true
CREATE TABLE auth.users (
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
--rollback DROP TABLE auth.users;

--changeset zeroship:auth-identities splitStatements:true
CREATE TABLE auth.identities (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    provider      TEXT NOT NULL,
    subject       TEXT NOT NULL,
    email_at_link CITEXT,
    raw_profile   JSONB,
    linked_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (provider, subject)
);
--rollback DROP TABLE auth.identities;

--changeset zeroship:auth-sessions splitStatements:true
CREATE TABLE auth.sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES auth.users(id),
    auth_method     TEXT NOT NULL,
    amr             TEXT[] NOT NULL,
    acr             TEXT,
    auth_time       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    credential_version BIGINT NOT NULL DEFAULT 0,
    idle_expires_at TIMESTAMPTZ NOT NULL,
    abs_expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at      TIMESTAMPTZ
);
--rollback DROP TABLE auth.sessions;

--changeset zeroship:auth-magic-links splitStatements:true
CREATE TABLE auth.magic_links (
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
CREATE INDEX auth_magic_email_idx ON auth.magic_links (email);
--rollback DROP TABLE auth.magic_links;

--changeset zeroship:auth-magic-completions splitStatements:true
CREATE TABLE auth.magic_completions (
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
    ON auth.magic_completions (expires_at) WHERE consumed_at IS NULL;
--rollback DROP TABLE auth.magic_completions;

--changeset zeroship:auth-email-verifications splitStatements:true
CREATE TABLE auth.email_verifications (
    token_hash  BYTEA PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
    email       CITEXT NOT NULL,
    issued_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at  TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ
);
--rollback DROP TABLE auth.email_verifications;

--changeset zeroship:auth-email-suppressions splitStatements:true
CREATE TABLE auth.email_suppressions (
    email         CITEXT PRIMARY KEY,
    reason        TEXT NOT NULL,
    suppressed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    provider_msg  TEXT
);
--rollback DROP TABLE auth.email_suppressions;

--changeset zeroship:auth-rate-limits splitStatements:true
CREATE TABLE auth.rate_limits (
    bucket_key TEXT PRIMARY KEY,
    tokens     REAL NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
--rollback DROP TABLE auth.rate_limits;

--changeset zeroship:auth-audit-events splitStatements:true
CREATE TABLE auth.audit_events (
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
CREATE INDEX auth_audit_user_idx  ON auth.audit_events (user_id, occurred_at);
CREATE INDEX auth_audit_event_idx ON auth.audit_events (event_type, occurred_at);
--rollback DROP TABLE auth.audit_events;

-- Append-only guard for auth.audit_events: a trigger function that raises on
-- any UPDATE/DELETE/TRUNCATE, the three triggers wiring it, the PUBLIC revoke,
-- and the per-role revoke loop (guards on role existence). splitStatements:false
-- because the function body and the DO block contain `;` inside `$$`.
--changeset zeroship:auth-audit-events-guard splitStatements:false
CREATE OR REPLACE FUNCTION auth.audit_events_block_tamper()
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
     RAISE EXCEPTION 'auth.audit_events is append-only'
         USING ERRCODE = 'insufficient_privilege';
 END
 $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS audit_events_block_update ON auth.audit_events;
CREATE TRIGGER audit_events_block_update
    BEFORE UPDATE ON auth.audit_events
    FOR EACH ROW EXECUTE FUNCTION auth.audit_events_block_tamper();
DROP TRIGGER IF EXISTS audit_events_block_delete ON auth.audit_events;
CREATE TRIGGER audit_events_block_delete
    BEFORE DELETE ON auth.audit_events
    FOR EACH ROW EXECUTE FUNCTION auth.audit_events_block_tamper();
DROP TRIGGER IF EXISTS audit_events_block_truncate ON auth.audit_events;
CREATE TRIGGER audit_events_block_truncate
    BEFORE TRUNCATE ON auth.audit_events
    FOR EACH STATEMENT EXECUTE FUNCTION auth.audit_events_block_tamper();
REVOKE UPDATE, DELETE, TRUNCATE ON TABLE auth.audit_events FROM PUBLIC;
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
                 'REVOKE UPDATE, DELETE, TRUNCATE ON TABLE auth.audit_events FROM %I',
                 role_name
             );
         END IF;
     END LOOP;
 END
 $$;
--rollback DROP TRIGGER IF EXISTS audit_events_block_truncate ON auth.audit_events;
--rollback DROP TRIGGER IF EXISTS audit_events_block_delete ON auth.audit_events;
--rollback DROP TRIGGER IF EXISTS audit_events_block_update ON auth.audit_events;
--rollback DROP FUNCTION IF EXISTS auth.audit_events_block_tamper();

--changeset zeroship:auth-gateway-sessions splitStatements:true
CREATE TABLE auth.gateway_sessions (
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
CREATE INDEX auth_gateway_sessions_app_idx ON auth.gateway_sessions (app_id, user_id);
CREATE INDEX auth_gateway_sessions_idle_idx ON auth.gateway_sessions (idle_expires_at) WHERE revoked_at IS NULL;
--rollback DROP TABLE auth.gateway_sessions;

--changeset zeroship:auth-dpop-jti splitStatements:true
CREATE TABLE auth.dpop_jti (
    jti         TEXT PRIMARY KEY,
    inserted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX auth_dpop_jti_inserted_idx
    ON auth.dpop_jti (inserted_at);
--rollback DROP TABLE auth.dpop_jti;

--changeset zeroship:auth-console-sessions splitStatements:true
CREATE TABLE auth.console_sessions (
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
CREATE INDEX auth_console_sessions_user_idx ON auth.console_sessions (user_id);
CREATE INDEX auth_console_sessions_idle_idx ON auth.console_sessions (idle_expires_at) WHERE revoked_at IS NULL;
--rollback DROP TABLE auth.console_sessions;

--changeset zeroship:auth-cron-state splitStatements:true
CREATE TABLE auth.cron_state (
    key             TEXT PRIMARY KEY,
    last_rotated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    notes           TEXT
);
--rollback DROP TABLE auth.cron_state;

-- Missing tables (cron bug fix). The token-sweep and jwk-rotation cron tasks
-- reference these but no migration ever created them. Inferred from
-- crates/core/src/wrapper_revocation.rs and
-- crates/auth/src/cron/{token_sweep,jwk_rotation}.rs.
--changeset zeroship:auth-wrapper-revoked-subjects splitStatements:true
CREATE TABLE auth.wrapper_revoked_subjects (
    subject    UUID PRIMARY KEY,
    revoked_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE auth.wrapper_revoked_subjects;

--changeset zeroship:auth-jwk-key-state splitStatements:true
CREATE TABLE auth.jwk_key_state (
    set_name   TEXT NOT NULL,
    kid        TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (set_name, kid)
);
--rollback DROP TABLE auth.jwk_key_state;

-- Cross-node access-token revocation family marker (spec §8.5). The
-- PRIMARY revocation mechanism for the Bearer arm: keyed PER-APP on
-- (client_id, sub) so revoking a user on app A leaves app B untouched.
-- `sub` is TEXT to hold BOTH the wrapper's pws_ pairwise subject and the
-- raw-Hydra global UUID. The Bearer arm rejects a token when a row exists
-- with revoked_after > token.iat. See crates/core/src/wrapper_revocation.rs
-- (revoke_family / is_family_revoked_since / sweep_expired_families).
--changeset zeroship:auth-token-revocations splitStatements:true
CREATE TABLE auth.token_revocations (
    client_id     TEXT        NOT NULL,
    sub           TEXT        NOT NULL,
    revoked_after TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (client_id, sub)
);
--rollback DROP TABLE auth.token_revocations;
