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
