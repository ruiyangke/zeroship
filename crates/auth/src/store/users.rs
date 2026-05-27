//! `auth.users` CRUD.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: uuid::Uuid,
    pub email: String,
    pub email_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub name: String,
    pub avatar_url: Option<String>,
    pub password_hash: Option<String>,
    pub locked_until: Option<chrono::DateTime<chrono::Utc>>,
}

/// Look up a user by email. Returns `None` if not found.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn find_by_email(conn: &Client, email: &str) -> Result<Option<UserRow>> {
    let rows = conn
        .query(
            "SELECT id, email::text, email_verified_at, name, avatar_url, password_hash, locked_until \
             FROM auth.users WHERE email = $1",
            &[&email],
        )
        .await
        .map_err(|e| AuthError::Db(format!("users find_by_email: {e}")))?;
    Ok(rows.first().map(row_to_user))
}

/// Look up a user by their primary key (`auth.users.id`).
///
/// The argument is the UUID rendered as a hyphenated string — that's the
/// shape `accept_login` stamps into the hydra session as `subject`, and
/// what the consent challenge then surfaces back via `info.subject`.
///
/// Returns `Ok(None)` if the string doesn't parse as a UUID OR if no row
/// matches. Callers handling consent flows treat both as "subject unknown
/// to us" → fall back to a minimal `id_token` (sub-only, claims omitted).
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure (parse failure is NOT an error —
/// it's a `None`, since the subject string is attacker-influenced).
pub async fn find_by_id(conn: &Client, id: &str) -> Result<Option<UserRow>> {
    let Ok(uuid) = uuid::Uuid::parse_str(id) else {
        return Ok(None);
    };
    let rows = conn
        .query(
            "SELECT id, email::text, email_verified_at, name, avatar_url, password_hash, locked_until \
             FROM auth.users WHERE id = $1",
            &[&uuid],
        )
        .await
        .map_err(|e| AuthError::Db(format!("users find_by_id: {e}")))?;
    Ok(rows.first().map(row_to_user))
}

/// Insert a new user. Returns the created row.
///
/// # Errors
///
/// Returns `AuthError::Db` on conflict (e.g., duplicate email) or other PG failure.
pub async fn create(
    conn: &Client,
    email: &str,
    name: &str,
    password_hash: Option<&str>,
) -> Result<UserRow> {
    let rows = conn
        .query(
            "INSERT INTO auth.users (email, name, password_hash) \
             VALUES ($1, $2, $3) \
             RETURNING id, email::text, email_verified_at, name, avatar_url, password_hash, locked_until",
            &[&email, &name, &password_hash],
        )
        .await
        .map_err(|e| {
            if let Some(db_err) = e.as_db_error() {
                if db_err.code().code() == "23505" {
                    return AuthError::Db("email already registered".into());
                }
            }
            AuthError::Db(format!("users create: {e}"))
        })?;
    let row = rows
        .first()
        .ok_or_else(|| AuthError::Db("users create: no row returned".into()))?;
    Ok(row_to_user(row))
}

/// Replace `auth.users.password_hash` with a fresh PHC string (Argon2id).
/// Used by the password-reset flow (P5-U6) to set a new credential after
/// a valid reset-token redeem.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn update_password_hash(conn: &Client, id: uuid::Uuid, phc: &str) -> Result<()> {
    conn.execute(
        "UPDATE auth.users SET password_hash = $1, updated_at = NOW() WHERE id = $2",
        &[&phc, &id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("users update_password_hash: {e}")))?;
    Ok(())
}

/// Bump `last_login_at` to `NOW()`.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn touch_last_login(conn: &Client, id: uuid::Uuid) -> Result<()> {
    conn.execute(
        "UPDATE auth.users SET last_login_at = NOW(), updated_at = NOW() WHERE id = $1",
        &[&id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("users touch_last_login: {e}")))?;
    Ok(())
}

fn row_to_user(row: &compio_postgres::Row) -> UserRow {
    UserRow {
        id: row.get("id"),
        email: row.get::<_, String>("email"),
        email_verified_at: row.try_get("email_verified_at").ok(),
        name: row.get("name"),
        avatar_url: row.try_get("avatar_url").ok(),
        password_hash: row.try_get("password_hash").ok(),
        locked_until: row.try_get("locked_until").ok(),
    }
}
