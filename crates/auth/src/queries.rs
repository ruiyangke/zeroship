//! All SQL queries for the auth service — one file, easy to audit.

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

pub const CREATE_UUID_EXTENSION: &str =
    "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"";

pub const CREATE_USERS_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS auth_users (
        id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
        email TEXT UNIQUE NOT NULL,
        name TEXT NOT NULL,
        avatar_url TEXT,
        password_hash TEXT,
        email_verified BOOLEAN DEFAULT FALSE,
        created_at TIMESTAMPTZ DEFAULT NOW(),
        updated_at TIMESTAMPTZ DEFAULT NOW(),
        last_login TIMESTAMPTZ
    )";

pub const CREATE_CONSENTS_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS auth_app_consents (
        id SERIAL PRIMARY KEY,
        user_id UUID NOT NULL REFERENCES auth_users(id),
        app_id UUID NOT NULL,
        granted_at TIMESTAMPTZ DEFAULT NOW(),
        revoked_at TIMESTAMPTZ,
        UNIQUE (user_id, app_id)
    )";

// ---------------------------------------------------------------------------
// User columns — single source of truth for SELECT projections
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// User queries
// ---------------------------------------------------------------------------

pub const INSERT_USER: &str =
    "INSERT INTO auth_users (email, name, password_hash) VALUES ($1, $2, $3)";

pub const INSERT_OAUTH_USER: &str =
    "INSERT INTO auth_users (email, name, avatar_url, email_verified) VALUES ($1, $2, NULLIF($3, ''), TRUE)";

pub const SELECT_USER_BY_EMAIL: &str =
    "SELECT id, email, name, avatar_url, email_verified FROM auth_users WHERE email = $1";

pub const SELECT_USER_BY_EMAIL_WITH_PASSWORD: &str =
    "SELECT id, email, name, avatar_url, email_verified, password_hash FROM auth_users WHERE email = $1";

pub const SELECT_USER_BY_ID: &str =
    "SELECT id, email, name, avatar_url, email_verified FROM auth_users WHERE id = $1::uuid";

pub const UPDATE_LAST_LOGIN: &str =
    "UPDATE auth_users SET last_login = NOW() WHERE id = $1::uuid";

// ---------------------------------------------------------------------------
// Consent queries
// ---------------------------------------------------------------------------

pub const UPSERT_CONSENT: &str =
    "INSERT INTO auth_app_consents (user_id, app_id) VALUES ($1::uuid, $2::uuid) \
     ON CONFLICT (user_id, app_id) DO UPDATE SET revoked_at = NULL, granted_at = NOW()";

pub const INSERT_CONSENT_IF_NOT_EXISTS: &str =
    "INSERT INTO auth_app_consents (user_id, app_id) VALUES ($1::uuid, $2::uuid) \
     ON CONFLICT (user_id, app_id) DO NOTHING";

pub const SELECT_CONSENT: &str =
    "SELECT id FROM auth_app_consents \
     WHERE user_id = $1::uuid AND app_id = $2::uuid AND revoked_at IS NULL";
