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
        created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        last_login_at     TIMESTAMPTZ
    )",

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
        consumed_at TIMESTAMPTZ
    )",
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
        consumed_at      TIMESTAMPTZ
    )",
    "ALTER TABLE auth.magic_completions \
        ADD COLUMN IF NOT EXISTS attempts SMALLINT NOT NULL DEFAULT 0",
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
    migrate_legacy_auth_users(conn).await?;
    Ok(())
}

fn first_line(stmt: &str) -> &str {
    stmt.lines().next().unwrap_or("").trim()
}

/// One-shot migration from the legacy public-schema `auth_users` tables
/// (created by the orphaned `crates/auth/migrations/001_*.sql` before P1-U1
/// deleted that crate) to `auth.users`.
///
/// Idempotent: if `public.auth_users` doesn't exist, this is a no-op.
/// Conflict resolution: rows whose email is already in `auth.users`
/// (e.g., from a fresh Phase 1+2 signup) are skipped.
///
/// # Errors
///
/// Returns `AuthError::Db` on any PG failure.
pub async fn migrate_legacy_auth_users(conn: &Client) -> Result<()> {
    // Check existence of the legacy table.
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = 'auth_users'",
            &[],
        )
        .await
        .map_err(|e| AuthError::Db(format!("legacy check: {e}")))?;
    if rows.is_empty() {
        tracing::debug!("legacy public.auth_users not present; skipping migration");
        return Ok(());
    }

    tracing::info!("legacy public.auth_users found; migrating to auth.users");

    // INSERT-then-DROP in a single batch_execute call so it's atomic per
    // session. Note: batch_execute runs in implicit transaction;
    // compio-postgres preserves the all-or-nothing semantics.
    //
    // Column mapping rationale:
    //   - public.auth_users had `email_verified BOOLEAN`; we map to
    //     `auth.users.email_verified_at` (TIMESTAMPTZ) using the
    //     `created_at` value when verified, NULL otherwise.
    //   - All other columns map by name.
    //   - `password_hash` may be a bcrypt hash from the legacy flow;
    //     after migration, those users will need to re-set their password
    //     via the magic-link flow (Phase 5). For Phase 3, just preserve.
    //   - `ON CONFLICT (email) DO NOTHING` so re-running is safe.
    conn.batch_execute(
        "INSERT INTO auth.users \
            (id, email, name, avatar_url, password_hash, \
             email_verified_at, created_at, last_login_at) \
         SELECT id, email::citext, name, avatar_url, password_hash, \
                CASE WHEN email_verified THEN created_at ELSE NULL END, \
                created_at, last_login \
         FROM public.auth_users \
         ON CONFLICT (email) DO NOTHING; \
         \
         DROP TABLE IF EXISTS public.auth_users CASCADE; \
         DROP TABLE IF EXISTS public.auth_app_consents CASCADE; \
         DROP TABLE IF EXISTS public.auth_sessions CASCADE;",
    )
    .await
    .map_err(|e| AuthError::Db(format!("legacy migrate: {e}")))?;

    tracing::info!("legacy migration complete; public.auth_users dropped");
    Ok(())
}
