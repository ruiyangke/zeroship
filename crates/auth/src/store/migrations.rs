//! `auth.*` schema migrations. Run on every boot; statements are idempotent.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

const STATEMENTS: &[&str] = &[
    // Schema
    "CREATE SCHEMA IF NOT EXISTS auth",
    // citext for case-insensitive emails
    "CREATE EXTENSION IF NOT EXISTS citext",
    "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"",

    // 5.1 users
    "CREATE TABLE IF NOT EXISTS auth.users (
        id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        email             CITEXT UNIQUE NOT NULL,
        email_verified_at TIMESTAMPTZ,
        name              TEXT NOT NULL,
        avatar_url        TEXT,
        password_hash     TEXT,
        locked_until      TIMESTAMPTZ,
        disabled_at       TIMESTAMPTZ,
        created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        last_login_at     TIMESTAMPTZ
    )",
    "ALTER TABLE auth.users \
        ADD COLUMN IF NOT EXISTS disabled_at TIMESTAMPTZ",

    // 5.2 identities
    "CREATE TABLE IF NOT EXISTS auth.identities (
        id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        user_id       UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
        provider      TEXT NOT NULL,
        subject       TEXT NOT NULL,
        email_at_link CITEXT,
        raw_profile   JSONB,
        linked_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        UNIQUE (provider, subject)
    )",

    // 5.3 sessions (our IdP login session)
    "CREATE TABLE IF NOT EXISTS auth.sessions (
        id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        user_id         UUID NOT NULL REFERENCES auth.users(id),
        auth_method     TEXT NOT NULL,
        amr             TEXT[] NOT NULL,
        acr             TEXT,
        auth_time       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        idle_expires_at TIMESTAMPTZ NOT NULL,
        abs_expires_at  TIMESTAMPTZ NOT NULL,
        revoked_at      TIMESTAMPTZ
    )",

    // 5.4 magic links
    "CREATE TABLE IF NOT EXISTS auth.magic_links (
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
    )",
    "ALTER TABLE auth.magic_links \
        ADD COLUMN IF NOT EXISTS consumed_pending_at TIMESTAMPTZ",
    "CREATE INDEX IF NOT EXISTS auth_magic_email_idx ON auth.magic_links (email)",

    // 5.4b magic-link cross-device completions — when the redeeming device's
    // `__Host-zsidp_magic_csrf` cookie doesn't match the requesting device's
    // (i.e., the user opened the email link on a different browser), we
    // surface a 6-digit code on the redeeming device and require the user
    // to type it back into the original requesting device. One row per
    // pending cross-device flow, keyed by the magic-link's CSRF nonce.
    "CREATE TABLE IF NOT EXISTS auth.magic_completions (
        csrf_nonce       TEXT PRIMARY KEY,
        code             TEXT NOT NULL,
        email            CITEXT NOT NULL,
        login_challenge  TEXT NOT NULL,
        attempts         SMALLINT NOT NULL DEFAULT 0,
        expires_at       TIMESTAMPTZ NOT NULL,
        consumed_pending_at TIMESTAMPTZ,
        consumed_at      TIMESTAMPTZ
    )",
    "ALTER TABLE auth.magic_completions \
        ADD COLUMN IF NOT EXISTS attempts SMALLINT NOT NULL DEFAULT 0",
    "ALTER TABLE auth.magic_completions \
        ADD COLUMN IF NOT EXISTS consumed_pending_at TIMESTAMPTZ",
    "CREATE INDEX IF NOT EXISTS auth_magic_completions_expires_idx \
        ON auth.magic_completions (expires_at) WHERE consumed_at IS NULL",

    // 5.5 email verifications
    "CREATE TABLE IF NOT EXISTS auth.email_verifications (
        token_hash  BYTEA PRIMARY KEY,
        user_id     UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
        email       CITEXT NOT NULL,
        issued_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        expires_at  TIMESTAMPTZ NOT NULL,
        consumed_at TIMESTAMPTZ
    )",

    // 5.6 email suppressions
    "CREATE TABLE IF NOT EXISTS auth.email_suppressions (
        email         CITEXT PRIMARY KEY,
        reason        TEXT NOT NULL,
        suppressed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        provider_msg  TEXT
    )",

    // 5.7 rate-limit buckets
    "CREATE TABLE IF NOT EXISTS auth.rate_limits (
        bucket_key TEXT PRIMARY KEY,
        tokens     REAL NOT NULL,
        updated_at TIMESTAMPTZ NOT NULL
    )",

    // 5.8 audit events
    "CREATE TABLE IF NOT EXISTS auth.audit_events (
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
    )",
    "CREATE INDEX IF NOT EXISTS auth_audit_user_idx  ON auth.audit_events (user_id, occurred_at)",
    "CREATE INDEX IF NOT EXISTS auth_audit_event_idx ON auth.audit_events (event_type, occurred_at)",

    // 5.9 gateway sessions — per-origin `__Host-zs_app_session` cookies
    // validated by the gateway. Distinct from `auth.sessions` (the IdP
    // login session on auth.zeroship.ai): one creator app = one row per
    // browser session per end user. Proposal §9.2.
    "CREATE TABLE IF NOT EXISTS auth.gateway_sessions (
        id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        user_id         TEXT NOT NULL,
        app_id          TEXT NOT NULL,
        email           CITEXT,
        name            TEXT,
        avatar_url      TEXT,
        email_verified  BOOLEAN NOT NULL DEFAULT FALSE,
        issued_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        idle_expires_at TIMESTAMPTZ NOT NULL,
        abs_expires_at  TIMESTAMPTZ NOT NULL,
        revoked_at      TIMESTAMPTZ
    )",
    "CREATE INDEX IF NOT EXISTS auth_gateway_sessions_app_idx ON auth.gateway_sessions (app_id, user_id)",
    "CREATE INDEX IF NOT EXISTS auth_gateway_sessions_idle_idx ON auth.gateway_sessions (idle_expires_at) WHERE revoked_at IS NULL",

    // 5.9b DPoP proof jti replay cache — shared by all gateway
    // processes so a proof replayed onto a sibling node is rejected
    // during the verifier freshness window.
    "CREATE TABLE IF NOT EXISTS auth.dpop_jti (
        jti         TEXT PRIMARY KEY,
        inserted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
    )",
    "CREATE INDEX IF NOT EXISTS auth_dpop_jti_inserted_idx
        ON auth.dpop_jti (inserted_at)",

    // 5.9c Wrapper-token subject revocation denylist. Back-channel
    // logout inserts a subject here; gateway wrapper verification
    // rejects wrappers whose iat is at or before revoked_at. Rows are
    // swept after the wrapper-token maximum lifetime plus cushion.
    "CREATE TABLE IF NOT EXISTS auth.wrapper_revoked_subjects (
        subject    UUID PRIMARY KEY,
        revoked_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
    )",
    "CREATE INDEX IF NOT EXISTS auth_wrapper_revoked_subjects_revoked_idx
        ON auth.wrapper_revoked_subjects (revoked_at)",

    // 5.10 console sessions — per-origin `__Host-zs_console_session`
    // cookies validated by the control plane (the OIDC RP for the
    // creator dashboard at `console.zeroship.ai`). Shape mirrors
    // `auth.gateway_sessions`, minus `app_id` because there is only
    // ever one console origin. Proposal §2.3 + §9.2.
    "CREATE TABLE IF NOT EXISTS auth.console_sessions (
        id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        user_id         TEXT NOT NULL,
        email           CITEXT,
        name            TEXT,
        avatar_url      TEXT,
        email_verified  BOOLEAN NOT NULL DEFAULT FALSE,
        issued_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        idle_expires_at TIMESTAMPTZ NOT NULL,
        abs_expires_at  TIMESTAMPTZ NOT NULL,
        revoked_at      TIMESTAMPTZ
    )",
    "CREATE INDEX IF NOT EXISTS auth_console_sessions_user_idx ON auth.console_sessions (user_id)",
    "CREATE INDEX IF NOT EXISTS auth_console_sessions_idle_idx ON auth.console_sessions (idle_expires_at) WHERE revoked_at IS NULL",

    // 5.11 cron_state — durable "last-ran" anchors for in-process cron
    // tasks (P6-U1: JWK rotation, keyed by hydra JWK set name). One row
    // per cron-managed resource. Read on every tick; upserted when the
    // task takes action (rotation/retirement etc.).
    "CREATE TABLE IF NOT EXISTS auth.cron_state (
        key             TEXT PRIMARY KEY,
        last_rotated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        notes           TEXT
    )",

    // P9-U5 authorization storage — platform RBAC, creator app
    // memberships, per-token policies, operator platform policies, and
    // authorization decision audit.
    "CREATE SCHEMA IF NOT EXISTS platform",
    "CREATE SCHEMA IF NOT EXISTS control",
    "CREATE TABLE IF NOT EXISTS platform.roles (
        user_id    UUID PRIMARY KEY REFERENCES auth.users(id) ON DELETE CASCADE,
        role       TEXT NOT NULL CHECK (role IN ('admin','support','billing','readonly')),
        granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        granted_by UUID REFERENCES auth.users(id)
    )",
    "CREATE TABLE IF NOT EXISTS control.app_members (
        app_id   TEXT NOT NULL,
        user_id  UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
        role     TEXT NOT NULL CHECK (role IN ('owner','editor','viewer')),
        added_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        added_by UUID REFERENCES auth.users(id),
        PRIMARY KEY (app_id, user_id)
    )",
    "CREATE INDEX IF NOT EXISTS app_members_user_idx ON control.app_members (user_id)",
    "CREATE TABLE IF NOT EXISTS control.permission_tokens (
        id           UUID PRIMARY KEY,
        owner_id     UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
        kind         TEXT NOT NULL CHECK (kind IN ('pat','oauth_grant')),
        client_id    TEXT,
        name         TEXT NOT NULL,
        policies     JSONB NOT NULL,
        policy_hash  TEXT NOT NULL,
        created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        expires_at   TIMESTAMPTZ,
        revoked_at   TIMESTAMPTZ,
        last_used_at TIMESTAMPTZ
    )",
    "CREATE INDEX IF NOT EXISTS permission_tokens_owner_active_idx
        ON control.permission_tokens (owner_id) WHERE revoked_at IS NULL",
    "CREATE INDEX IF NOT EXISTS permission_tokens_policies_gin_idx
        ON control.permission_tokens USING GIN (policies)",
    "CREATE TABLE IF NOT EXISTS control.platform_policies (
        id           TEXT PRIMARY KEY,
        cedar_source TEXT NOT NULL,
        enabled      BOOLEAN NOT NULL DEFAULT TRUE,
        updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        updated_by   UUID REFERENCES auth.users(id)
    )",
    "CREATE TABLE IF NOT EXISTS control.oauth_clients (
        client_id            TEXT PRIMARY KEY,
        client_name          TEXT NOT NULL,
        client_uri           TEXT,
        logo_uri             TEXT,
        redirect_uris        TEXT[] NOT NULL,
        scopes               TEXT[] NOT NULL,
        skip_consent         BOOLEAN NOT NULL DEFAULT FALSE,
        created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        created_by           UUID REFERENCES auth.users(id),
        hydra_client_id      TEXT NOT NULL
    )",
    "ALTER TABLE control.oauth_clients ALTER COLUMN created_by DROP NOT NULL",
    "CREATE TABLE IF NOT EXISTS control.oauth_grants (
        user_id          UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
        client_id        TEXT NOT NULL REFERENCES control.oauth_clients(client_id) ON DELETE CASCADE,
        granted_scopes   TEXT[] NOT NULL,
        granted_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        last_used_at     TIMESTAMPTZ,
        PRIMARY KEY (user_id, client_id)
    )",
    "CREATE INDEX IF NOT EXISTS oauth_grants_user_granted_idx
        ON control.oauth_grants (user_id, granted_at DESC)",
    "CREATE INDEX IF NOT EXISTS oauth_grants_client_idx
        ON control.oauth_grants (client_id)",
    "CREATE TABLE IF NOT EXISTS control.authz_decisions (
        id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        occurred_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        user_id          UUID,
        token_id         UUID,
        action           TEXT NOT NULL,
        resource_type    TEXT NOT NULL,
        resource_id      TEXT,
        decision         TEXT NOT NULL CHECK (decision IN ('allow','deny')),
        matched_policies TEXT[] NOT NULL DEFAULT '{}',
        request_ip       INET,
        request_id       TEXT
    )",
    "CREATE INDEX IF NOT EXISTS authz_decisions_occurred_idx
        ON control.authz_decisions (occurred_at DESC)",
    "CREATE INDEX IF NOT EXISTS authz_decisions_user_idx
        ON control.authz_decisions (user_id) WHERE user_id IS NOT NULL",
];

/// Apply all migrations in order. Each statement is idempotent and safe to
/// re-run on every boot.
///
/// # Errors
///
/// Returns [`AuthError::Db`] if any statement fails — typically because the
/// connecting role lacks `CREATE` on the database (the `citext` /
/// `uuid-ossp` extension installs need superuser or `pg_extension_owner`
/// on first boot) or the server is unreachable.
pub async fn migrate(conn: &Client) -> Result<()> {
    for stmt in STATEMENTS {
        conn.execute(*stmt, &[])
            .await
            .map_err(|e| AuthError::Db(format!("migration `{}`: {e}", first_line(stmt))))?;
    }
    Ok(())
}

fn first_line(stmt: &str) -> &str {
    stmt.lines().next().unwrap_or("").trim()
}
