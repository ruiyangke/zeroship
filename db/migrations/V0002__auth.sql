-- auth.* schema. Transcribed verbatim from crates/auth/src/store/migrations.rs
-- (the STATEMENTS array). Every object reference is fully schema-qualified.
-- zeroship.users is created first; all other auth tables FK into it.

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

CREATE TABLE zeroship.federated_identities (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    provider      TEXT NOT NULL,
    subject       TEXT NOT NULL,
    email_at_link CITEXT,
    raw_profile   JSONB,
    linked_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (provider, subject)
);

CREATE TABLE zeroship.idp_sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    auth_method     TEXT NOT NULL,
    amr             TEXT[] NOT NULL,
    acr             TEXT,
    auth_time       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    credential_version BIGINT NOT NULL DEFAULT 0,
    idle_expires_at TIMESTAMPTZ NOT NULL,
    abs_expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at      TIMESTAMPTZ
);

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

CREATE TABLE zeroship.email_verifications (
    token_hash  BYTEA PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    email       CITEXT NOT NULL,
    issued_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at  TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ
);

CREATE TABLE zeroship.email_suppressions (
    email         CITEXT PRIMARY KEY,
    reason        TEXT NOT NULL,
    suppressed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    provider_msg  TEXT
);

CREATE TABLE zeroship.rate_limits (
    bucket_key TEXT PRIMARY KEY,
    tokens     REAL NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE zeroship.audit_events (
    id            BIGSERIAL PRIMARY KEY,
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    event_type    TEXT NOT NULL,
    outcome       TEXT NOT NULL,
    actor_user_id UUID,
    client_id     TEXT,
    request_id    TEXT,
    ip            INET,
    user_agent    TEXT,
    auth_method   TEXT,
    detail        JSONB
);
CREATE INDEX auth_audit_user_idx  ON zeroship.audit_events (actor_user_id, occurred_at);
CREATE INDEX auth_audit_event_idx ON zeroship.audit_events (event_type, occurred_at);

-- Append-only guard for zeroship.audit_events: a trigger function that raises on
-- any UPDATE/DELETE/TRUNCATE, the three triggers wiring it, the PUBLIC revoke,
-- and the per-role revoke loop (guards on role existence). splitStatements:false
-- because the function body and the DO block contain `;` inside `$$`.
CREATE OR REPLACE FUNCTION zeroship.audit_events_block_tamper()
 RETURNS trigger AS $$
 BEGIN






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
         'zeroship_app'
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

CREATE TABLE zeroship.gateway_sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The gateway session store (gateway::sessions) parses the subject into a
    -- Uuid and binds/reads it as UUID, so the column is UUID (not TEXT).
    user_id         UUID NOT NULL,
    -- app_id is UUID. The FK into zeroship.apps(id) ON DELETE CASCADE is added
    -- in 0004_control.sql (changeset control-apps-cascade-fks) — NOT here —
    -- because zeroship.apps is created there (0004 > 0002 lexicographically), so
    -- the constraint can only resolve once apps exists. Deleting an app then
    -- atomically tears down its gateway sessions via that real FK (single
    -- physical DB, one `zeroship` schema — no companion sweep).
    app_id          UUID NOT NULL,
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

CREATE TABLE zeroship.dpop_jti (
    jti         TEXT PRIMARY KEY,
    inserted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX auth_dpop_jti_inserted_idx
    ON zeroship.dpop_jti (inserted_at);

CREATE TABLE zeroship.cron_state (
    key             TEXT PRIMARY KEY,
    last_rotated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    notes           TEXT
);

-- Missing tables (cron bug fix). The jwk-rotation cron task references this
-- but no migration ever created it. Inferred from
-- crates/auth/src/cron/jwk_rotation.rs.
--
-- (The former zeroship.wrapper_revoked_subjects global subject denylist was
-- removed in Batch A M2 — it was write-only dead code after the per-app
-- pws_ revocation cutover; the per-app zeroship.token_revocations family marker
-- below is the sole wrapper-token revocation primitive.)
CREATE TABLE zeroship.jwk_key_state (
    set_name   TEXT NOT NULL,
    kid        TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (set_name, kid)
);

-- Cross-node access-token revocation family marker (spec §8.5). The
-- PRIMARY revocation mechanism for the Bearer arm: keyed PER-APP on
-- (client_id, sub) so revoking a user on app A leaves app B untouched.
-- `sub` is TEXT to hold BOTH the wrapper's pws_ pairwise subject and the
-- global UUID subject. The Bearer arm rejects a token when a row exists
-- with revoked_after > token.iat. See crates/core/src/wrapper_revocation.rs
-- (revoke_family / is_family_revoked_since / sweep_expired_families).
CREATE TABLE zeroship.token_revocations (
    client_id     TEXT        NOT NULL,
    sub           TEXT        NOT NULL,
    revoked_after TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (client_id, sub)
);
CREATE INDEX auth_token_revocations_revoked_after_idx
    ON zeroship.token_revocations (revoked_after);
