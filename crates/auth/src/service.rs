//! Auth service — user registration, login, JWT issuance, consent management.

use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use zeroship_core::typed_id;
use zeroship_pg::Conn;

use crate::queries as sql;

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
    /// Connect to the database, run migrations, return an `AuthService`.
    pub async fn new(db_url: &str, jwt_secret: &str) -> Result<Self, String> {
        let mut conn = Conn::connect(db_url).await.map_err(|e| e.to_string())?;

        conn.execute(sql::CREATE_UUID_EXTENSION, &[]).await.map_err(|e| format!("auth migration: {e}"))?;
        conn.execute(sql::CREATE_USERS_TABLE, &[]).await.map_err(|e| format!("auth migration: {e}"))?;
        conn.execute(sql::CREATE_CONSENTS_TABLE, &[]).await.map_err(|e| format!("auth migration: {e}"))?;

        let _ = conn.close().await;
        Ok(Self { db_url: db_url.to_string(), jwt_secret: jwt_secret.to_string() })
    }

    /// Open a fresh connection.
    async fn conn(&self) -> Result<Conn, String> {
        Conn::connect(&self.db_url).await.map_err(|e| e.to_string())
    }

    // -- Registration ---------------------------------------------------------

    /// Register a new user with email + password.
    pub async fn register(&self, email: &str, password: &str, name: &str) -> Result<AuthUser, String> {
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

        conn.execute(sql::INSERT_USER, &[&email, &name, &hash.as_str()])
            .await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("duplicate key") || msg.contains("unique") || msg.contains("23505") {
                    "email already registered".to_string()
                } else {
                    format!("database: {msg}")
                }
            })?;

        let rows = conn.query(sql::SELECT_USER_BY_EMAIL, &[&email]).await.map_err(|e| format!("database: {e}"))?;
        rows.first().map(row_to_user).ok_or_else(|| "insert ok but read-back failed".to_string())
    }

    /// Find or create a user from an OAuth provider profile. No password.
    pub async fn find_or_create_oauth_user(&self, email: &str, name: &str, avatar_url: Option<&str>) -> Result<AuthUser, String> {
        let mut conn = self.conn().await?;

        let rows = conn.query(sql::SELECT_USER_BY_EMAIL, &[&email]).await.map_err(|e| format!("database: {e}"))?;
        if let Some(row) = rows.first() {
            return Ok(row_to_user(row));
        }

        let avatar = avatar_url.unwrap_or("");
        conn.execute(sql::INSERT_OAUTH_USER, &[&email, &name, &avatar]).await.map_err(|e| format!("database: {e}"))?;

        let rows = conn.query(sql::SELECT_USER_BY_EMAIL, &[&email]).await.map_err(|e| format!("database: {e}"))?;
        rows.first().map(row_to_user).ok_or_else(|| "insert ok but read-back failed".to_string())
    }

    // -- Login ----------------------------------------------------------------

    /// Authenticate with email + password and issue a JWT scoped to an app.
    pub async fn login(&self, email: &str, password: &str, app_id: &str) -> Result<LoginResult, String> {
        let mut conn = self.conn().await?;

        let rows = conn.query(sql::SELECT_USER_BY_EMAIL_WITH_PASSWORD, &[&email]).await.map_err(|e| format!("database: {e}"))?;
        let row = rows.first().ok_or("invalid email or password")?;

        let stored_hash: Option<String> = row.try_get("password_hash").ok();
        match stored_hash {
            None => return Err("this account uses social login — no password set".into()),
            Some(hash) => {
                if !bcrypt::verify(password, &hash).map_err(|e| format!("bcrypt verify: {e}"))? {
                    return Err("invalid email or password".into());
                }
            }
        }

        let raw_uuid: String = row.get("id");
        let user = row_to_user(row);

        let _ = conn.execute(sql::UPDATE_LAST_LOGIN, &[&raw_uuid.as_str()]).await;
        let _ = conn.execute(sql::INSERT_CONSENT_IF_NOT_EXISTS, &[&raw_uuid.as_str(), &app_id]).await;

        let token = self.issue_token(&user, app_id)?;
        Ok(LoginResult { user, token })
    }

    // -- Token verification ---------------------------------------------------

    /// Verify and decode a JWT.
    pub fn verify_token(&self, token: &str) -> Result<TokenClaims, String> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_required_spec_claims(&["sub", "app", "exp", "iat"]);

        let data = decode::<TokenClaims>(token, &DecodingKey::from_secret(self.jwt_secret.as_bytes()), &validation)
            .map_err(|e| format!("invalid token: {e}"))?;
        Ok(data.claims)
    }

    // -- User lookup ----------------------------------------------------------

    /// Get a user by typed ID (`usr_...`).
    pub async fn get_user(&self, user_id: &str) -> Result<AuthUser, String> {
        let uuid = user_id_to_uuid(user_id)?;
        let mut conn = self.conn().await?;

        let rows = conn.query(sql::SELECT_USER_BY_ID, &[&uuid.as_str()]).await.map_err(|e| format!("database: {e}"))?;
        rows.first().map(row_to_user).ok_or_else(|| "user not found".to_string())
    }

    // -- Consent management ---------------------------------------------------

    /// Check whether a user has granted consent to an app.
    pub async fn has_consent(&self, user_id: &str, app_id: &str) -> Result<bool, String> {
        let uuid = user_id_to_uuid(user_id)?;
        let mut conn = self.conn().await?;

        let rows = conn.query(sql::SELECT_CONSENT, &[&uuid.as_str(), &app_id]).await.map_err(|e| format!("database: {e}"))?;
        Ok(!rows.is_empty())
    }

    /// Grant consent for a user to an app. Idempotent.
    pub async fn grant_consent(&self, user_id: &str, app_id: &str) -> Result<(), String> {
        let uuid = user_id_to_uuid(user_id)?;
        let mut conn = self.conn().await?;

        conn.execute(sql::UPSERT_CONSENT, &[&uuid.as_str(), &app_id]).await.map_err(|e| format!("database: {e}"))?;
        Ok(())
    }

    // -- Token issuance -------------------------------------------------------

    /// Issue a JWT for the given user scoped to an app. Expires in 24h.
    pub fn issue_token(&self, user: &AuthUser, app_id: &str) -> Result<String, String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| format!("system time: {e}"))?;
        let iat = now.as_secs() as usize;

        let claims = TokenClaims {
            sub: user.id.clone(),
            app: app_id.to_string(),
            email: user.email.clone(),
            name: user.name.clone(),
            avatar: user.avatar_url.clone(),
            email_verified: user.email_verified,
            exp: iat + 86400,
            iat,
        };

        encode(&Header::new(Algorithm::HS256), &claims, &EncodingKey::from_secret(self.jwt_secret.as_bytes()))
            .map_err(|e| format!("jwt encode: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a query row into an `AuthUser` (PG UUID → typed ID).
fn row_to_user(row: &zeroship_pg::Row) -> AuthUser {
    let raw_uuid: String = row.get("id");
    let id = typed_id::from_uuid_string(typed_id::USER_PREFIX, &raw_uuid).unwrap_or(raw_uuid);
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
