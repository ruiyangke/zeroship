//! Auth service — user registration, login, JWT issuance, consent management.

use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use zeroship_pg::Conn;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String, // UUID
    pub email: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String, // user UUID
    pub app: String, // app UUID
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
                password_hash TEXT NOT NULL,
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

        conn.execute(
            "CREATE TABLE IF NOT EXISTS auth_sessions (
                id SERIAL PRIMARY KEY,
                user_id UUID NOT NULL REFERENCES auth_users(id),
                app_id UUID NOT NULL,
                token_hash TEXT NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                created_at TIMESTAMPTZ DEFAULT NOW()
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
                "SELECT id, email, name, avatar_url FROM auth_users WHERE email = $1",
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
    /// app. Also records a session and grants consent if not already present.
    pub async fn login(
        &self,
        email: &str,
        password: &str,
        app_id: &str,
    ) -> Result<LoginResult, String> {
        let mut conn = self.conn().await?;

        let rows = conn
            .query(
                "SELECT id, email, name, avatar_url, password_hash FROM auth_users WHERE email = $1",
                &[&email],
            )
            .await
            .map_err(|e| format!("database: {e}"))?;

        let row = rows.first().ok_or("invalid email or password")?;
        let stored_hash: String = row.get("password_hash");

        let valid =
            bcrypt::verify(password, &stored_hash).map_err(|e| format!("bcrypt verify: {e}"))?;
        if !valid {
            return Err("invalid email or password".into());
        }

        let user = row_to_user(row);

        // Update last_login
        let _ = conn
            .execute(
                "UPDATE auth_users SET last_login = NOW() WHERE id = $1::uuid",
                &[&user.id.as_str()],
            )
            .await;

        // Auto-grant consent on login
        let _ = conn
            .execute(
                "INSERT INTO auth_app_consents (user_id, app_id) VALUES ($1::uuid, $2::uuid) \
                 ON CONFLICT (user_id, app_id) DO NOTHING",
                &[&user.id.as_str(), &app_id],
            )
            .await;

        let token = self.issue_token(&user, app_id)?;

        // Record session
        let token_hash = sha2_hex(&token);
        let _ = conn
            .execute(
                "INSERT INTO auth_sessions (user_id, app_id, token_hash, expires_at) \
                 VALUES ($1::uuid, $2::uuid, $3, NOW() + INTERVAL '24 hours')",
                &[&user.id.as_str(), &app_id, &token_hash.as_str()],
            )
            .await;

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

    /// Get a user by ID.
    pub async fn get_user(&self, user_id: &str) -> Result<AuthUser, String> {
        let mut conn = self.conn().await?;

        let rows = conn
            .query(
                "SELECT id, email, name, avatar_url FROM auth_users WHERE id = $1::uuid",
                &[&user_id],
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
        let mut conn = self.conn().await?;

        let rows = conn
            .query(
                "SELECT id FROM auth_app_consents \
                 WHERE user_id = $1::uuid AND app_id = $2::uuid AND revoked_at IS NULL",
                &[&user_id, &app_id],
            )
            .await
            .map_err(|e| format!("database: {e}"))?;

        Ok(!rows.is_empty())
    }

    /// Grant consent for a user to an app. Idempotent (re-grants if revoked).
    pub async fn grant_consent(&self, user_id: &str, app_id: &str) -> Result<(), String> {
        let mut conn = self.conn().await?;

        conn.execute(
            "INSERT INTO auth_app_consents (user_id, app_id) VALUES ($1::uuid, $2::uuid) \
             ON CONFLICT (user_id, app_id) DO UPDATE SET revoked_at = NULL, granted_at = NOW()",
            &[&user_id, &app_id],
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
fn row_to_user(row: &zeroship_pg::Row) -> AuthUser {
    AuthUser {
        id: row.get("id"), // UUID comes back as String from text-format params
        email: row.get("email"),
        name: row.get("name"),
        avatar_url: row.get("avatar_url"),
    }
}

/// SHA-256 hex digest of input.
fn sha2_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}
