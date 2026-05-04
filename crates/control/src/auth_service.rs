//! Auth service — user registration, login, JWT issuance, OAuth.
//!
//! Two flavors of session live in the same `auth_users` table:
//!
//! 1. **Creator session** (no `app_id` on the token). Used by the
//!    dashboard's login flow — JWT subject is the user, scope is
//!    "platform". Grants access to the creator's own apps.
//!
//! 2. **End-user-of-app session** (`app_id` set on the token). Used
//!    by deployed apps that opt in to platform-managed auth — JWT
//!    subject is the user, scope is the specific app uuid. Drives
//!    the gateway's `ZeroShip-User` injection.
//!
//! Both flavors share the user table, password hashing (bcrypt),
//! OAuth linking, and session bookkeeping.

use std::time::{SystemTime, UNIX_EPOCH};

use compio_postgres::{Client, NoTls};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use zeroship_core::typed_id;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String, // typed id (`usr_...`)
    pub email: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

/// Issued by `issue_token`. The `app` scope is `None` for creator
/// (dashboard) sessions and `Some(app_uuid)` for end-user-of-app
/// sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String,
    /// App scope. None = creator session (dashboard).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub app: Option<String>,
    pub email: String,
    pub name: String,
    pub avatar: Option<String>,
    pub exp: usize,
    pub iat: usize,
}

#[derive(Debug)]
pub struct LoginResult {
    pub user: AuthUser,
    pub token: String,
}

#[derive(Debug, Clone, Copy)]
pub enum OAuthProvider { Google }

impl OAuthProvider {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Google => "google" }
    }
}

// ---------------------------------------------------------------------------
// AuthService
// ---------------------------------------------------------------------------

pub struct AuthService {
    db_url: String,
    jwt_secret: String,
}

impl std::fmt::Debug for AuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthService")
            .field("db_url", &"<redacted>")
            .field("jwt_secret", &"<redacted>")
            .finish()
    }
}

impl AuthService {
    pub async fn new(db_url: &str, jwt_secret: &str) -> Result<Self, String> {
        let conn = open_conn(db_url).await.map_err(|e| e.to_string())?;

        conn.execute("CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"", &[])
            .await.map_err(|e| format!("auth migration: {e}"))?;

        // Initial table — kept compatible with the original schema.
        // password_hash is NOT NULL here, but the next ALTER drops the
        // constraint so OAuth-only users (no password) can exist.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS auth_users (
                id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
                email TEXT UNIQUE NOT NULL,
                name TEXT NOT NULL,
                avatar_url TEXT,
                password_hash TEXT NOT NULL,
                email_verified BOOLEAN DEFAULT FALSE,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                updated_at TIMESTAMPTZ DEFAULT NOW(),
                last_login TIMESTAMPTZ
            )", &[],
        ).await.map_err(|e| format!("auth migration: {e}"))?;

        // Make password_hash nullable so OAuth-only signups work.
        conn.execute(
            "ALTER TABLE auth_users ALTER COLUMN password_hash DROP NOT NULL",
            &[],
        ).await.ok(); // ignore if already nullable

        // OAuth linkage columns.
        conn.execute(
            "ALTER TABLE auth_users ADD COLUMN IF NOT EXISTS oauth_provider TEXT",
            &[],
        ).await.map_err(|e| format!("auth migration: {e}"))?;
        conn.execute(
            "ALTER TABLE auth_users ADD COLUMN IF NOT EXISTS oauth_subject TEXT",
            &[],
        ).await.map_err(|e| format!("auth migration: {e}"))?;
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS auth_users_oauth_idx \
             ON auth_users (oauth_provider, oauth_subject) \
             WHERE oauth_provider IS NOT NULL",
            &[],
        ).await.map_err(|e| format!("auth migration: {e}"))?;

        // Existing supporting tables — kept for the per-app flavor.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS auth_app_consents (
                id SERIAL PRIMARY KEY,
                user_id UUID NOT NULL REFERENCES auth_users(id),
                app_id UUID NOT NULL,
                granted_at TIMESTAMPTZ DEFAULT NOW(),
                revoked_at TIMESTAMPTZ,
                UNIQUE (user_id, app_id)
            )", &[],
        ).await.map_err(|e| format!("auth migration: {e}"))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS auth_sessions (
                id SERIAL PRIMARY KEY,
                user_id UUID NOT NULL REFERENCES auth_users(id),
                app_id UUID,
                token_hash TEXT NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                created_at TIMESTAMPTZ DEFAULT NOW()
            )", &[],
        ).await.map_err(|e| format!("auth migration: {e}"))?;

        // Make app_id nullable on sessions too (for creator sessions).
        conn.execute(
            "ALTER TABLE auth_sessions ALTER COLUMN app_id DROP NOT NULL",
            &[],
        ).await.ok();

        drop(conn);

        Ok(Self {
            db_url: db_url.to_string(),
            jwt_secret: jwt_secret.to_string(),
        })
    }

    async fn conn(&self) -> Result<Client, String> {
        open_conn(&self.db_url).await.map_err(|e| e.to_string())
    }

    // -- Registration --------------------------------------------------------

    pub async fn register(
        &self,
        email: &str,
        password: &str,
        name: &str,
    ) -> Result<AuthUser, String> {
        if email.is_empty() || !email.contains('@') {
            return Err("invalid email".into());
        }
        if password.len() < 8 {
            return Err("password must be at least 8 characters".into());
        }
        if name.is_empty() {
            return Err("name is required".into());
        }

        let hash = bcrypt::hash(password, 12).map_err(|e| format!("bcrypt: {e}"))?;
        let conn = self.conn().await?;

        // INSERT … RETURNING id::text — single round-trip. We use a
        // plain text scalar query and read column 0 as String to
        // sidestep the postgres-types UUID-decode panic the row.get
        // path was hitting on this driver.
        let rows = conn.query(
            "INSERT INTO auth_users (email, name, password_hash) VALUES ($1, $2, $3) \
             RETURNING id::text AS id",
            &[&email, &name, &hash.as_str()],
        ).await.map_err(|e| {
            // Surface SQLSTATE explicitly so the dup-email path
            // doesn't depend on the driver's prose error format.
            if let Some(db_err) = e.as_db_error() {
                if db_err.code().code() == "23505" {
                    return "email already registered".to_string();
                }
            }
            let msg = e.to_string();
            if msg.contains("duplicate key") || msg.contains("unique") || msg.contains("23505") {
                "email already registered".to_string()
            } else { format!("database: {msg}") }
        })?;

        let raw_uuid: String = rows.first()
            .ok_or_else(|| "insert ok but RETURNING was empty".to_string())?
            .try_get::<_, String>("id")
            .map_err(|e| format!("read id back: {e}"))?;

        Ok(AuthUser {
            id: typed_id::from_uuid_string(typed_id::USER_PREFIX, &raw_uuid).unwrap_or(raw_uuid),
            email: email.to_string(),
            name: name.to_string(),
            avatar_url: None,
        })
    }

    // -- Login (email + password) --------------------------------------------

    /// Authenticate with email + password. `app_id` is optional —
    /// `None` issues a creator session (dashboard scope), `Some(uuid)`
    /// issues an end-user-of-app session.
    pub async fn login(
        &self,
        email: &str,
        password: &str,
        app_id: Option<&str>,
    ) -> Result<LoginResult, String> {
        let conn = self.conn().await?;

        let rows = conn.query(
            "SELECT id::text AS id, email, name, avatar_url, password_hash \
             FROM auth_users WHERE email = $1",
            &[&email],
        ).await.map_err(|e| format!("database: {e}"))?;

        let row = rows.first().ok_or("invalid email or password")?;
        let stored_hash: Option<String> = row.try_get("password_hash").ok();

        // OAuth-only accounts have no password_hash and can't login this way.
        let stored_hash = stored_hash.ok_or(
            "this account is registered via Google — sign in with that provider",
        )?;

        let valid =
            bcrypt::verify(password, &stored_hash).map_err(|e| format!("bcrypt verify: {e}"))?;
        if !valid {
            return Err("invalid email or password".into());
        }

        self.finish_login(&conn, row, app_id).await
    }

    // -- OAuth login / link --------------------------------------------------

    /// Find-or-create a user from an OAuth provider's verified ID-token
    /// claims. Issues a creator-session token (no app_id) by default.
    /// If `app_id` is set, scopes the token to an app instead.
    #[allow(clippy::too_many_arguments)]
    pub async fn oauth_login(
        &self,
        provider: OAuthProvider,
        subject: &str,
        email: &str,
        name: &str,
        avatar_url: Option<&str>,
        app_id: Option<&str>,
    ) -> Result<LoginResult, String> {
        if subject.is_empty() {
            return Err("oauth subject empty".into());
        }
        let provider_str = provider.as_str();
        let conn = self.conn().await?;

        // 1. Look up by (provider, subject) — already linked.
        let rows = conn.query(
            "SELECT id::text AS id, email, name, avatar_url FROM auth_users \
             WHERE oauth_provider = $1 AND oauth_subject = $2",
            &[&provider_str, &subject],
        ).await.map_err(|e| format!("database: {e}"))?;

        if let Some(row) = rows.first() {
            return self.finish_login(&conn, row, app_id).await;
        }

        // 2. Look up by email — link existing email/password user to OAuth.
        let rows = conn.query(
            "SELECT id::text AS id, email, name, avatar_url FROM auth_users WHERE email = $1",
            &[&email],
        ).await.map_err(|e| format!("database: {e}"))?;

        if let Some(row) = rows.first() {
            let raw_uuid: String = row.get("id");
            let _ = conn.execute(
                "UPDATE auth_users SET oauth_provider = $1, oauth_subject = $2, \
                                       avatar_url = COALESCE(avatar_url, $3) \
                 WHERE id = $4::uuid",
                &[&provider_str, &subject, &avatar_url, &raw_uuid.as_str()],
            ).await;
            return self.finish_login(&conn, row, app_id).await;
        }

        // 3. Brand-new user — create with OAuth fields.
        conn.execute(
            "INSERT INTO auth_users (email, name, avatar_url, oauth_provider, oauth_subject, email_verified) \
             VALUES ($1, $2, $3, $4, $5, TRUE)",
            &[&email, &name, &avatar_url, &provider_str, &subject],
        ).await.map_err(|e| format!("oauth insert: {e}"))?;

        let rows = conn.query(
            "SELECT id::text AS id, email, name, avatar_url FROM auth_users WHERE email = $1",
            &[&email],
        ).await.map_err(|e| format!("database: {e}"))?;

        let row = rows.first().ok_or("oauth insert ok but read-back failed")?;
        self.finish_login(&conn, row, app_id).await
    }

    async fn finish_login(
        &self,
        conn: &Client,
        row: &compio_postgres::Row,
        app_id: Option<&str>,
    ) -> Result<LoginResult, String> {
        let raw_uuid: String = row.get("id");
        let user = row_to_user(row);

        let _ = conn.execute(
            "UPDATE auth_users SET last_login = NOW() WHERE id = $1::uuid",
            &[&raw_uuid.as_str()],
        ).await;

        if let Some(app) = app_id {
            let _ = conn.execute(
                "INSERT INTO auth_app_consents (user_id, app_id) VALUES ($1::uuid, $2::uuid) \
                 ON CONFLICT (user_id, app_id) DO NOTHING",
                &[&raw_uuid.as_str(), &app],
            ).await;
        }

        let token = self.issue_token(&user, app_id)?;
        let token_hash = sha2_hex(&token);
        let _ = conn.execute(
            "INSERT INTO auth_sessions (user_id, app_id, token_hash, expires_at) \
             VALUES ($1::uuid, $2::uuid, $3, NOW() + INTERVAL '24 hours')",
            &[&raw_uuid.as_str(), &app_id, &token_hash.as_str()],
        ).await;

        Ok(LoginResult { user, token })
    }

    // -- Token verification --------------------------------------------------

    pub fn verify_token(&self, token: &str) -> Result<TokenClaims, String> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_required_spec_claims(&["sub", "exp", "iat"]);

        let data = decode::<TokenClaims>(
            token,
            &DecodingKey::from_secret(self.jwt_secret.as_bytes()),
            &validation,
        ).map_err(|e| format!("invalid token: {e}"))?;

        Ok(data.claims)
    }

    // -- User lookup ---------------------------------------------------------

    pub async fn get_user(&self, user_id: &str) -> Result<AuthUser, String> {
        let uuid_str = user_id_to_uuid(user_id)?;
        let conn = self.conn().await?;

        // We pass the UUID as TEXT and cast in SQL — avoids the
        // postgres-types `&str` → `uuid` parameter-coercion path
        // that was tripping on this driver.
        let rows = conn.query(
            "SELECT id::text AS id, email, name, avatar_url \
             FROM auth_users WHERE id::text = $1",
            &[&uuid_str],
        ).await.map_err(|e| format!("database: {e}"))?;

        rows.first().map(row_to_user)
            .ok_or_else(|| "user not found".to_string())
    }

    // -- Consent management --------------------------------------------------

    pub async fn has_consent(&self, user_id: &str, app_id: &str) -> Result<bool, String> {
        let uuid = user_id_to_uuid(user_id)?;
        let conn = self.conn().await?;

        let rows = conn.query(
            "SELECT id FROM auth_app_consents \
             WHERE user_id = $1::uuid AND app_id = $2::uuid AND revoked_at IS NULL",
            &[&uuid.as_str(), &app_id],
        ).await.map_err(|e| format!("database: {e}"))?;

        Ok(!rows.is_empty())
    }

    pub async fn grant_consent(&self, user_id: &str, app_id: &str) -> Result<(), String> {
        let uuid = user_id_to_uuid(user_id)?;
        let conn = self.conn().await?;

        conn.execute(
            "INSERT INTO auth_app_consents (user_id, app_id) VALUES ($1::uuid, $2::uuid) \
             ON CONFLICT (user_id, app_id) DO UPDATE SET revoked_at = NULL, granted_at = NOW()",
            &[&uuid.as_str(), &app_id],
        ).await.map_err(|e| format!("database: {e}"))?;

        Ok(())
    }

    // -- Token issuance ------------------------------------------------------

    pub fn issue_token(&self, user: &AuthUser, app_id: Option<&str>) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("system time: {e}"))?;

        let iat = now.as_secs() as usize;
        let exp = iat + 86400; // 24 hours

        let claims = TokenClaims {
            sub: user.id.clone(),
            app: app_id.map(|s| s.to_string()),
            email: user.email.clone(),
            name: user.name.clone(),
            avatar: user.avatar_url.clone(),
            exp,
            iat,
        };

        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(self.jwt_secret.as_bytes()),
        ).map_err(|e| format!("jwt encode: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn open_conn(url: &str) -> Result<Client, compio_postgres::Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "control/auth: pg connection error");
        }
    }).detach();
    Ok(client)
}

fn row_to_user(row: &compio_postgres::Row) -> AuthUser {
    // Reads id as String — every auth_service query selects
    // `id::text AS id` so this is always a TEXT column. Other
    // fields are TEXT natively. Each is read defensively (try_get)
    // so a single bad field can't take down the whole login flow.
    let id: String = row.try_get::<_, String>("id")
        .or_else(|_| row.try_get::<_, uuid::Uuid>("id").map(|u| u.to_string()))
        .unwrap_or_default();
    let id = typed_id::from_uuid_string(typed_id::USER_PREFIX, &id).unwrap_or(id);
    AuthUser {
        id,
        email: row.try_get::<_, String>("email").unwrap_or_default(),
        name: row.try_get::<_, String>("name").unwrap_or_default(),
        avatar_url: row.try_get::<_, Option<String>>("avatar_url").ok().flatten(),
    }
}

fn user_id_to_uuid(typed: &str) -> Result<String, String> {
    typed_id::to_uuid_string(typed)
}

fn sha2_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}
