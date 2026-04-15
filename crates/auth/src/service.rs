//! Auth service — user registration, login, JWT issuance, consent management.

use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use zeroship_core::typed_id;
use zeroship_pg::Conn;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String, // UUID → typed ID (usr_...)
    pub email: String,
    pub name: String,
    pub avatar_url: Option<String>,
    pub email_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String, // user UUID → typed ID (usr_...)
    pub app: String, // app UUID
    pub email: String,
    pub name: String,
    pub avatar: Option<String>,
    pub email_verified: bool,
    pub exp: usize,
    pub iat: usize,
}

#[derive(Debug)]
pub struct LoginResult {
    pub user: AuthUser,
    pub token: String,
}

// ---------------------------------------------------------------------------
// AuthService
// ---------------------------------------------------------------------------

/// Handles user registration, login, JWT issuance, and consent management.
/// Stores the DB URL and JWT secret; creates a fresh connection per operation
/// (same pattern as `Registry`).
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
    /// Connect to the database, run auth schema migrations, and return an
    /// `AuthService`.
    pub async fn new(db_url: &str, jwt_secret: &str) -> Result<Self, String> {
        let mut conn = Conn::connect(db_url).await.map_err(|e| e.to_string())?;

        conn.execute(
            "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"",
            &[],
        )
        .await
        .map_err(|e| format!("auth migration: {e}"))?;

        conn.execute(
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
            )",
            &[],
        )
        .await
        .map_err(|e| format!("auth migration: {e}"))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS auth_app_consents (
                id SERIAL PRIMARY KEY,
                user_id UUID NOT NULL REFERENCES auth_users(id),
                app_id UUID NOT NULL,
                granted_at TIMESTAMPTZ DEFAULT NOW(),
                revoked_at TIMESTAMPTZ,
                UNIQUE (user_id, app_id)
            )",
            &[],
        )
        .await
        .map_err(|e| format!("auth migration: {e}"))?;

        let _ = conn.close().await;

        Ok(Self {
            db_url: db_url.to_string(),
            jwt_secret: jwt_secret.to_string(),
        })
    }

    /// Open a fresh connection.
    async fn conn(&self) -> Result<Conn, String> {
        Conn::connect(&self.db_url)
            .await
            .map_err(|e| e.to_string())
    }

    // -- Registration ---------------------------------------------------------

    /// Register a new user with email + password. Returns the created user.
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

        let mut conn = self.conn().await?;

        conn.execute(
            "INSERT INTO auth_users (email, name, password_hash) VALUES ($1, $2, $3)",
            &[&email, &name, &hash.as_str()],
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("duplicate key") || msg.contains("unique") || msg.contains("23505") {
                "email already registered".to_string()
            } else {
                format!("database: {msg}")
            }
        })?;

        let rows = conn
            .query(
                "SELECT id, email, name, avatar_url, email_verified FROM auth_users WHERE email = $1",
                &[&email],
            )
            .await
            .map_err(|e| format!("database: {e}"))?;

        rows.first()
            .map(row_to_user)
            .ok_or_else(|| "insert ok but read-back failed".to_string())
    }

    /// Find or create a user from an OAuth provider profile. No password.
    /// Used by the OAuth callback flow — Google/GitHub provide email + name + avatar.
    pub async fn find_or_create_oauth_user(
        &self,
        email: &str,
        name: &str,
        avatar_url: Option<&str>,
    ) -> Result<AuthUser, String> {
        let mut conn = self.conn().await?;

        // Try to find existing user by email
        let rows = conn
            .query(
                "SELECT id, email, name, avatar_url, email_verified FROM auth_users WHERE email = $1",
                &[&email],
            )
            .await
            .map_err(|e| format!("database: {e}"))?;

        if let Some(row) = rows.first() {
            return Ok(row_to_user(row));
        }

        // Create new user — no password, email verified (provider confirmed it)
        let avatar = avatar_url.unwrap_or("");
        conn.execute(
            "INSERT INTO auth_users (email, name, avatar_url, email_verified) VALUES ($1, $2, NULLIF($3, ''), TRUE)",
            &[&email, &name, &avatar],
        )
        .await
        .map_err(|e| format!("database: {e}"))?;

        let rows = conn
            .query(
                "SELECT id, email, name, avatar_url, email_verified FROM auth_users WHERE email = $1",
                &[&email],
            )
            .await
            .map_err(|e| format!("database: {e}"))?;

        rows.first()
            .map(row_to_user)
            .ok_or_else(|| "insert ok but read-back failed".to_string())
    }

    // -- Login ----------------------------------------------------------------

    /// Authenticate with email + password and issue a JWT scoped to the given
    /// app. Grants consent if not already present. JWT is the session — no
    /// server-side session storage.
    pub async fn login(
        &self,
        email: &str,
        password: &str,
        app_id: &str,
    ) -> Result<LoginResult, String> {
        let mut conn = self.conn().await?;

        let rows = conn
            .query(
                "SELECT id, email, name, avatar_url, email_verified, password_hash FROM auth_users WHERE email = $1",
                &[&email],
            )
            .await
            .map_err(|e| format!("database: {e}"))?;

        let row = rows.first().ok_or("invalid email or password")?;
        let stored_hash: Option<String> = row.try_get("password_hash").ok();

        match stored_hash {
            None => return Err("this account uses social login — no password set".into()),
            Some(hash) => {
                let valid = bcrypt::verify(password, &hash)
                    .map_err(|e| format!("bcrypt verify: {e}"))?;
                if !valid {
                    return Err("invalid email or password".into());
                }
            }
        }

        let raw_uuid: String = row.get("id"); // raw UUID for internal queries
        let user = row_to_user(row);

        // Update last_login
        let _ = conn
            .execute(
                "UPDATE auth_users SET last_login = NOW() WHERE id = $1::uuid",
                &[&raw_uuid.as_str()],
            )
            .await;

        // Auto-grant consent on login
        let _ = conn
            .execute(
                "INSERT INTO auth_app_consents (user_id, app_id) VALUES ($1::uuid, $2::uuid) \
                 ON CONFLICT (user_id, app_id) DO NOTHING",
                &[&raw_uuid.as_str(), &app_id],
            )
            .await;

        let token = self.issue_token(&user, app_id)?;

        Ok(LoginResult { user, token })
    }

    // -- Token verification ---------------------------------------------------

    /// Verify and decode a JWT. Returns the claims if valid.
    pub fn verify_token(&self, token: &str) -> Result<TokenClaims, String> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_required_spec_claims(&["sub", "app", "exp", "iat"]);

        let data = decode::<TokenClaims>(
            token,
            &DecodingKey::from_secret(self.jwt_secret.as_bytes()),
            &validation,
        )
        .map_err(|e| format!("invalid token: {e}"))?;

        Ok(data.claims)
    }

    // -- User lookup ----------------------------------------------------------

    /// Get a user by typed ID (`usr_...`).
    pub async fn get_user(&self, user_id: &str) -> Result<AuthUser, String> {
        let uuid = user_id_to_uuid(user_id)?;
        let mut conn = self.conn().await?;

        let rows = conn
            .query(
                "SELECT id, email, name, avatar_url, email_verified FROM auth_users WHERE id = $1::uuid",
                &[&uuid.as_str()],
            )
            .await
            .map_err(|e| format!("database: {e}"))?;

        rows.first()
            .map(row_to_user)
            .ok_or_else(|| "user not found".to_string())
    }

    // -- Consent management ---------------------------------------------------

    /// Check whether a user has granted consent to an app.
    pub async fn has_consent(&self, user_id: &str, app_id: &str) -> Result<bool, String> {
        let uuid = user_id_to_uuid(user_id)?;
        let mut conn = self.conn().await?;

        let rows = conn
            .query(
                "SELECT id FROM auth_app_consents \
                 WHERE user_id = $1::uuid AND app_id = $2::uuid AND revoked_at IS NULL",
                &[&uuid.as_str(), &app_id],
            )
            .await
            .map_err(|e| format!("database: {e}"))?;

        Ok(!rows.is_empty())
    }

    /// Grant consent for a user to an app. Idempotent (re-grants if revoked).
    pub async fn grant_consent(&self, user_id: &str, app_id: &str) -> Result<(), String> {
        let uuid = user_id_to_uuid(user_id)?;
        let mut conn = self.conn().await?;

        conn.execute(
            "INSERT INTO auth_app_consents (user_id, app_id) VALUES ($1::uuid, $2::uuid) \
             ON CONFLICT (user_id, app_id) DO UPDATE SET revoked_at = NULL, granted_at = NOW()",
            &[&uuid.as_str(), &app_id],
        )
        .await
        .map_err(|e| format!("database: {e}"))?;

        Ok(())
    }

    // -- Token issuance -------------------------------------------------------

    /// Issue a JWT for the given user scoped to an app. Token expires in 24h.
    pub fn issue_token(&self, user: &AuthUser, app_id: &str) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("system time: {e}"))?;

        let iat = now.as_secs() as usize;
        let exp = iat + 86400; // 24 hours

        let claims = TokenClaims {
            sub: user.id.clone(),
            app: app_id.to_string(),
            email: user.email.clone(),
            name: user.name.clone(),
            avatar: user.avatar_url.clone(),
            email_verified: user.email_verified,
            exp,
            iat,
        };

        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(self.jwt_secret.as_bytes()),
        )
        .map_err(|e| format!("jwt encode: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a query row into an `AuthUser`.
/// PG stores raw UUID; we encode it as a typed ID (`usr_` + base62).
fn row_to_user(row: &zeroship_pg::Row) -> AuthUser {
    let raw_uuid: String = row.get("id");
    let id = typed_id::from_uuid_string(typed_id::USER_PREFIX, &raw_uuid)
        .unwrap_or(raw_uuid); // fallback to raw if encoding fails
    AuthUser {
        id,
        email: row.get("email"),
        name: row.get("name"),
        avatar_url: row.get("avatar_url"),
        email_verified: row.get("email_verified"),
    }
}

/// Convert a typed user ID (`usr_...`) to raw UUID string for PG queries.
fn user_id_to_uuid(typed: &str) -> Result<String, String> {
    typed_id::to_uuid_string(typed)
}
