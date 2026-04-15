//! All SQL queries for the auth service — one file, easy to audit.
//! Migrations live in crates/auth/migrations/*.sql — applied before deployment.

// ---------------------------------------------------------------------------
// User queries
// ---------------------------------------------------------------------------

pub const INSERT_USER: &str =
    "INSERT INTO auth_users (id, email, name, password_hash) VALUES ($1::uuid, $2, $3, $4)";

pub const INSERT_OAUTH_USER: &str =
    "INSERT INTO auth_users (id, email, name, avatar_url, email_verified) VALUES ($1::uuid, $2, $3, NULLIF($4, ''), TRUE)";

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
