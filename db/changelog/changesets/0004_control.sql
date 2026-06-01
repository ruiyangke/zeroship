--liquibase formatted sql

-- control.* schema. Transcribed from crates/control/src/registry.rs
-- (Registry::new) for the 10 application tables, and from
-- crates/auth/src/store/migrations.rs for the 6 authz/oauth tables.
-- All object references are fully schema-qualified.
--
-- Collapse / drop rules applied (per the design spec):
--   * ALTER … ADD COLUMN IF NOT EXISTS folded into the final column list.
--   * control_migrations table + keyed mechanism: dropped entirely.
--   * app_secrets_aad_v1 data wipe: dropped (no production data).
--   * creator_account_history open-row backfill: dropped (table + unique
--     partial index only).
--   * payouts: keep the inline named CHECKs; the idempotent
--     DO $$ pg_constraint re-add block is dropped.
--   * oauth_clients.created_by: declared nullable (ALTER … DROP NOT NULL
--     collapses in).
--   * app_audit: net = base columns (the DROP COLUMN actor is a no-op here).
--
-- FK order: oauth_clients before oauth_grants; creator_accounts before payouts.
-- The authz/oauth tables FK into zeroship.users, so this runs after 0002_auth.sql.

--changeset zeroship:control-apps splitStatements:true
CREATE TABLE zeroship.apps (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL UNIQUE,
    plan_id TEXT NOT NULL DEFAULT 'free',
    deploy_hash TEXT,
    api_key TEXT NOT NULL,
    api_key_hash TEXT NOT NULL DEFAULT '',
    env_version BIGINT NOT NULL DEFAULT 0,
    suspended BOOLEAN NOT NULL DEFAULT FALSE,
    audit_locked BOOLEAN NOT NULL DEFAULT FALSE,
    manifest_json TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
--rollback DROP TABLE zeroship.apps;

--changeset zeroship:control-usage splitStatements:true
CREATE TABLE zeroship.app_usage (
    app_id UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    resource TEXT NOT NULL,
    value BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (app_id, resource)
);
--rollback DROP TABLE zeroship.app_usage;

--changeset zeroship:control-usage-history splitStatements:true
CREATE TABLE zeroship.app_usage_history (
    app_id UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    period TEXT NOT NULL,
    counters JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (app_id, period)
);
CREATE INDEX idx_app_usage_history_app ON zeroship.app_usage_history(app_id, period);
--rollback DROP TABLE zeroship.app_usage_history;

--changeset zeroship:control-app-vars splitStatements:true
CREATE TABLE zeroship.app_vars (
    app_id UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    key_name TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, key_name)
);
--rollback DROP TABLE zeroship.app_vars;

--changeset zeroship:control-app-secrets splitStatements:true
CREATE TABLE zeroship.app_secrets (
    app_id UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    key_name TEXT NOT NULL,
    ciphertext BYTEA NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, key_name)
);
--rollback DROP TABLE zeroship.app_secrets;

--changeset zeroship:control-app-env-expose splitStatements:true
CREATE TABLE zeroship.app_env_expose (
    app_id UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    key_name TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (app_id, key_name)
);
--rollback DROP TABLE zeroship.app_env_expose;

--changeset zeroship:control-creator-accounts splitStatements:true
CREATE TABLE zeroship.creator_accounts (
    creator_id UUID PRIMARY KEY REFERENCES zeroship.users(id),
    stripe_account_id TEXT NOT NULL,
    onboarded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    unlinked_at TIMESTAMPTZ
);
--rollback DROP TABLE zeroship.creator_accounts;

--changeset zeroship:control-creator-account-history splitStatements:true
CREATE TABLE zeroship.creator_account_history (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    creator_id UUID NOT NULL,
    stripe_account_id TEXT NOT NULL,
    linked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    unlinked_at TIMESTAMPTZ
);
CREATE INDEX idx_creator_account_history_creator
    ON zeroship.creator_account_history(creator_id, linked_at DESC);
CREATE UNIQUE INDEX idx_creator_account_history_one_open
    ON zeroship.creator_account_history(creator_id)
    WHERE unlinked_at IS NULL;
--rollback DROP TABLE zeroship.creator_account_history;

--changeset zeroship:control-payouts splitStatements:true
CREATE TABLE zeroship.payouts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    creator_id UUID NOT NULL REFERENCES zeroship.creator_accounts(creator_id) ON DELETE RESTRICT,
    event_id TEXT NOT NULL UNIQUE,
    event_type TEXT NOT NULL,
    gross_amount BIGINT NOT NULL
        CONSTRAINT control_payouts_gross_nonnegative CHECK (gross_amount >= 0),
    platform_fee BIGINT NOT NULL
        CONSTRAINT control_payouts_fee_nonnegative CHECK (platform_fee >= 0),
    net_amount BIGINT NOT NULL
        CONSTRAINT control_payouts_net_nonnegative CHECK (net_amount >= 0),
    currency TEXT NOT NULL
        CONSTRAINT control_payouts_currency_shape CHECK (currency ~ '^[a-z]{3}$'),
    occurred_at TIMESTAMPTZ NOT NULL,
    payload_hash BYTEA,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT control_payouts_fee_lte_gross CHECK (platform_fee <= gross_amount),
    CONSTRAINT control_payouts_net_matches_amounts CHECK (net_amount = gross_amount - platform_fee)
);
CREATE INDEX idx_payouts_creator_time ON zeroship.payouts(creator_id, occurred_at DESC);
--rollback DROP TABLE zeroship.payouts;

--changeset zeroship:control-app-audit splitStatements:true
CREATE TABLE zeroship.app_audit (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    app_id UUID,
    creator_id UUID,
    actor_user_id UUID,
    actor_token_id UUID,
    action TEXT NOT NULL,
    resource TEXT,
    source_ip INET,
    detail JSONB,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_app_audit_app_at ON zeroship.app_audit(app_id, occurred_at DESC);
CREATE INDEX idx_app_audit_creator_at ON zeroship.app_audit(creator_id, occurred_at DESC);
--rollback DROP TABLE zeroship.app_audit;

-- Append-only guard for zeroship.app_audit. The trigger function lives in the
-- zeroship schema (uniform with audit_events_block_tamper /
-- authz_decisions_block_tamper). splitStatements:false because the
-- function body and the DO block contain `;` inside `$$`.
--changeset zeroship:control-app-audit-guard splitStatements:false
CREATE OR REPLACE FUNCTION zeroship.app_audit_block_tamper()
 RETURNS trigger AS $$
 BEGIN
     RAISE EXCEPTION 'app_audit is append-only'
         USING ERRCODE = 'insufficient_privilege';
 END
 $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS app_audit_block_update ON zeroship.app_audit;
CREATE TRIGGER app_audit_block_update
    BEFORE UPDATE ON zeroship.app_audit
    FOR EACH ROW EXECUTE FUNCTION zeroship.app_audit_block_tamper();
DROP TRIGGER IF EXISTS app_audit_block_delete ON zeroship.app_audit;
CREATE TRIGGER app_audit_block_delete
    BEFORE DELETE ON zeroship.app_audit
    FOR EACH ROW EXECUTE FUNCTION zeroship.app_audit_block_tamper();
DROP TRIGGER IF EXISTS app_audit_block_truncate ON zeroship.app_audit;
CREATE TRIGGER app_audit_block_truncate
    BEFORE TRUNCATE ON zeroship.app_audit
    FOR EACH STATEMENT EXECUTE FUNCTION zeroship.app_audit_block_tamper();
REVOKE UPDATE, DELETE, TRUNCATE ON TABLE zeroship.app_audit FROM PUBLIC;
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
                 'REVOKE UPDATE, DELETE, TRUNCATE ON TABLE zeroship.app_audit FROM %I',
                 role_name
             );
         END IF;
     END LOOP;
 END
 $$;
--rollback DROP TRIGGER IF EXISTS app_audit_block_truncate ON zeroship.app_audit;
--rollback DROP TRIGGER IF EXISTS app_audit_block_delete ON zeroship.app_audit;
--rollback DROP TRIGGER IF EXISTS app_audit_block_update ON zeroship.app_audit;
--rollback DROP FUNCTION IF EXISTS zeroship.app_audit_block_tamper();

--changeset zeroship:control-app-members splitStatements:true
CREATE TABLE zeroship.app_members (
    -- app_id is UUID and FKs into zeroship.apps(id) ON DELETE CASCADE (apps is
    -- created earlier in THIS file, so the inline FK resolves): deleting an app
    -- atomically drops its membership rows.
    app_id   UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    user_id  UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    role     TEXT NOT NULL CHECK (role IN ('owner','editor','viewer')),
    added_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    added_by UUID REFERENCES zeroship.users(id),
    PRIMARY KEY (app_id, user_id)
);
CREATE INDEX app_members_user_idx ON zeroship.app_members (user_id);
--rollback DROP TABLE zeroship.app_members;

--changeset zeroship:control-permission-tokens splitStatements:true
CREATE TABLE zeroship.permission_tokens (
    id           UUID PRIMARY KEY,
    owner_id     UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('pat','oauth_grant')),
    client_id    TEXT,
    name         TEXT NOT NULL,
    policies     JSONB NOT NULL,
    policy_hash  TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at   TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ
);
CREATE INDEX permission_tokens_owner_active_idx
    ON zeroship.permission_tokens (owner_id) WHERE revoked_at IS NULL;
CREATE INDEX permission_tokens_policies_gin_idx
    ON zeroship.permission_tokens USING GIN (policies);
--rollback DROP TABLE zeroship.permission_tokens;

--changeset zeroship:control-platform-policies splitStatements:true
CREATE TABLE zeroship.platform_policies (
    id           TEXT PRIMARY KEY,
    cedar_source TEXT NOT NULL,
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by   UUID REFERENCES zeroship.users(id)
);
--rollback DROP TABLE zeroship.platform_policies;

--changeset zeroship:control-oauth-clients splitStatements:true
CREATE TABLE zeroship.oauth_clients (
    client_id            TEXT PRIMARY KEY,
    client_name          TEXT NOT NULL,
    client_uri           TEXT,
    logo_uri             TEXT,
    redirect_uris        TEXT[] NOT NULL,
    scopes               TEXT[] NOT NULL,
    skip_consent         BOOLEAN NOT NULL DEFAULT FALSE,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by           UUID REFERENCES zeroship.users(id),
    hydra_client_id      TEXT NOT NULL
);
--rollback DROP TABLE zeroship.oauth_clients;

--changeset zeroship:control-oauth-grants splitStatements:true
CREATE TABLE zeroship.oauth_grants (
    user_id          UUID NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,
    client_id        TEXT NOT NULL REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE,
    granted_scopes   TEXT[] NOT NULL,
    granted_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at     TIMESTAMPTZ,
    PRIMARY KEY (user_id, client_id)
);
CREATE INDEX oauth_grants_user_granted_idx
    ON zeroship.oauth_grants (user_id, granted_at DESC);
CREATE INDEX oauth_grants_client_idx
    ON zeroship.oauth_grants (client_id);
--rollback DROP TABLE zeroship.oauth_grants;

--changeset zeroship:control-authz-decisions splitStatements:true
CREATE TABLE zeroship.authz_decisions (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    occurred_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    actor_user_id    UUID,
    token_id         UUID,
    action           TEXT NOT NULL,
    resource_type    TEXT NOT NULL,
    resource_id      TEXT,
    decision         TEXT NOT NULL CHECK (decision IN ('allow','deny')),
    matched_policies TEXT[] NOT NULL DEFAULT '{}',
    request_ip       INET,
    request_id       TEXT
);
CREATE INDEX authz_decisions_occurred_idx
    ON zeroship.authz_decisions (occurred_at DESC);
CREATE INDEX authz_decisions_user_idx
    ON zeroship.authz_decisions (actor_user_id) WHERE actor_user_id IS NOT NULL;
--rollback DROP TABLE zeroship.authz_decisions;

-- Append-only guard for zeroship.authz_decisions. splitStatements:false because
-- the function body and the DO block contain `;` inside `$$`.
--changeset zeroship:control-authz-decisions-guard splitStatements:false
CREATE OR REPLACE FUNCTION zeroship.authz_decisions_block_tamper()
 RETURNS trigger AS $$
 BEGIN
     RAISE EXCEPTION 'zeroship.authz_decisions is append-only'
         USING ERRCODE = 'insufficient_privilege';
 END
 $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS authz_decisions_block_update ON zeroship.authz_decisions;
CREATE TRIGGER authz_decisions_block_update
    BEFORE UPDATE ON zeroship.authz_decisions
    FOR EACH ROW EXECUTE FUNCTION zeroship.authz_decisions_block_tamper();
DROP TRIGGER IF EXISTS authz_decisions_block_delete ON zeroship.authz_decisions;
CREATE TRIGGER authz_decisions_block_delete
    BEFORE DELETE ON zeroship.authz_decisions
    FOR EACH ROW EXECUTE FUNCTION zeroship.authz_decisions_block_tamper();
DROP TRIGGER IF EXISTS authz_decisions_block_truncate ON zeroship.authz_decisions;
CREATE TRIGGER authz_decisions_block_truncate
    BEFORE TRUNCATE ON zeroship.authz_decisions
    FOR EACH STATEMENT EXECUTE FUNCTION zeroship.authz_decisions_block_tamper();
REVOKE UPDATE, DELETE, TRUNCATE ON TABLE zeroship.authz_decisions FROM PUBLIC;
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
                 'REVOKE UPDATE, DELETE, TRUNCATE ON TABLE zeroship.authz_decisions FROM %I',
                 role_name
             );
         END IF;
     END LOOP;
 END
 $$;
--rollback DROP TRIGGER IF EXISTS authz_decisions_block_truncate ON zeroship.authz_decisions;
--rollback DROP TRIGGER IF EXISTS authz_decisions_block_delete ON zeroship.authz_decisions;
--rollback DROP TRIGGER IF EXISTS authz_decisions_block_update ON zeroship.authz_decisions;
--rollback DROP FUNCTION IF EXISTS zeroship.authz_decisions_block_tamper();

-- Deferred FK: zeroship.gateway_sessions.app_id → zeroship.apps(id) ON DELETE
-- CASCADE. The column is declared UUID in 0002_auth.sql, but the constraint
-- can only be added once zeroship.apps exists — which is HERE (0004 > 0002),
-- and after the apps table is created at the top of this file. With this FK,
-- deleting an app atomically tears down its gateway sessions (no companion
-- sweep, single physical DB / one `zeroship` schema).
--changeset zeroship:control-gateway-sessions-app-fk splitStatements:true
ALTER TABLE zeroship.gateway_sessions
    ADD CONSTRAINT gateway_sessions_app_id_fkey
    FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE;
--rollback ALTER TABLE zeroship.gateway_sessions DROP CONSTRAINT gateway_sessions_app_id_fkey;
