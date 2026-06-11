//! `zeroship.totp_credentials` + `zeroship.totp_backup_codes` CRUD (ISS-11).
//!
//! The TOTP shared secret is stored ENCRYPTED here as opaque BYTEA — this
//! module never decrypts; the crypto lives in [`crate::identity::totp`]. The
//! handlers wire the two together.
//!
//! Lifecycle:
//!
//!   - [`enroll`] upserts a PENDING credential (`confirmed_at = NULL`) with the
//!     encrypted secret. A re-enroll overwrites the pending (or even a
//!     confirmed) row's secret and resets it to pending.
//!   - [`confirm`] flips `confirmed_at` to NOW() and inserts the backup-code
//!     hashes — in ONE transaction so a half-confirmed state can't exist.
//!   - [`find`] / [`find_confirmed`] read the credential at login time.
//!   - [`disable`] deletes the credential (CASCADE/explicit also clears codes).
//!   - [`unused_backup_codes`] + [`mark_backup_code_used`] back the
//!     recovery-code redeem at login.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

/// A stored TOTP credential row. The secret is the still-ENCRYPTED BYTEA blob;
/// callers decrypt via [`crate::identity::totp::decrypt_secret`].
#[derive(Debug, Clone)]
pub struct TotpCredential {
    pub user_id: uuid::Uuid,
    pub encrypted_secret: Vec<u8>,
    pub confirmed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// One backup-code row (hash + redemption state).
#[derive(Debug, Clone)]
pub struct BackupCode {
    pub id: i64,
    pub code_hash: String,
    pub used_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn row_to_credential(row: &compio_postgres::Row) -> TotpCredential {
    TotpCredential {
        user_id: row.get("user_id"),
        encrypted_secret: row.get::<_, Vec<u8>>("encrypted_secret"),
        confirmed_at: row.try_get("confirmed_at").ok(),
    }
}

/// Upsert a PENDING credential for `user_id` with the (already-encrypted)
/// `encrypted_secret`. Resets `confirmed_at` to NULL — a fresh enrollment is
/// never active until [`confirm`] verifies the first code. Idempotent on
/// re-enroll (overwrites the secret).
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn enroll(
    conn: &Client,
    user_id: uuid::Uuid,
    encrypted_secret: &[u8],
) -> Result<()> {
    let blob = encrypted_secret.to_vec();
    conn.execute(
        "INSERT INTO zeroship.totp_credentials (user_id, encrypted_secret, confirmed_at) \
         VALUES ($1, $2, NULL) \
         ON CONFLICT (user_id) DO UPDATE \
            SET encrypted_secret = EXCLUDED.encrypted_secret, \
                confirmed_at = NULL, \
                created_at = NOW()",
        &[&user_id, &blob],
    )
    .await
    .map_err(|e| AuthError::Db(format!("totp enroll: {e}")))?;
    Ok(())
}

/// Read the credential row for `user_id` (confirmed or pending), or `None`.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn find(conn: &Client, user_id: uuid::Uuid) -> Result<Option<TotpCredential>> {
    let rows = conn
        .query(
            "SELECT user_id, encrypted_secret, confirmed_at \
             FROM zeroship.totp_credentials WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("totp find: {e}")))?;
    Ok(rows.first().map(row_to_credential))
}

/// Read the credential ONLY if it is CONFIRMED (active). The login challenge
/// uses this so a pending (un-confirmed) enrollment never gates login.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn find_confirmed(conn: &Client, user_id: uuid::Uuid) -> Result<Option<TotpCredential>> {
    Ok(find(conn, user_id)
        .await?
        .filter(|c| c.confirmed_at.is_some()))
}

/// True iff `user_id` has a CONFIRMED TOTP credential (the login-challenge gate).
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn is_enabled(conn: &Client, user_id: uuid::Uuid) -> Result<bool> {
    Ok(find_confirmed(conn, user_id).await?.is_some())
}

/// Confirm a pending enrollment: stamp `confirmed_at = NOW()` and insert the
/// backup-code hashes, atomically. Replaces any prior codes for the user (a
/// re-confirm reissues a fresh set). Returns `false` (and writes nothing) if
/// the user has no pending credential to confirm.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure (transaction rolled back).
pub async fn confirm(
    conn: &Client,
    user_id: uuid::Uuid,
    backup_code_hashes: &[String],
) -> Result<bool> {
    conn.execute("BEGIN", &[])
        .await
        .map_err(|e| AuthError::Db(format!("totp confirm begin: {e}")))?;
    let result = confirm_tx(conn, user_id, backup_code_hashes).await;
    match result {
        Ok(value) => {
            conn.execute("COMMIT", &[])
                .await
                .map_err(|e| AuthError::Db(format!("totp confirm commit: {e}")))?;
            Ok(value)
        }
        Err(e) => {
            if let Err(rb) = conn.execute("ROLLBACK", &[]).await {
                tracing::error!(error = %rb, "totp confirm rollback failed");
            }
            Err(e)
        }
    }
}

async fn confirm_tx(
    conn: &Client,
    user_id: uuid::Uuid,
    backup_code_hashes: &[String],
) -> Result<bool> {
    // Only confirm a row that exists AND is still pending OR re-confirming. We
    // gate on existence: a missing credential → nothing to confirm.
    let updated = conn
        .execute(
            "UPDATE zeroship.totp_credentials \
             SET confirmed_at = NOW() \
             WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("totp confirm update: {e}")))?;
    if updated == 0 {
        return Ok(false);
    }

    // Fresh code set: drop any prior codes, then insert the new hashes.
    conn.execute(
        "DELETE FROM zeroship.totp_backup_codes WHERE user_id = $1",
        &[&user_id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("totp confirm clear codes: {e}")))?;

    for hash in backup_code_hashes {
        conn.execute(
            "INSERT INTO zeroship.totp_backup_codes (user_id, code_hash) VALUES ($1, $2)",
            &[&user_id, hash],
        )
        .await
        .map_err(|e| AuthError::Db(format!("totp confirm insert code: {e}")))?;
    }
    Ok(true)
}

/// Delete `user_id`'s TOTP credential and all backup codes (disable 2FA).
/// Returns `true` if a credential row existed. The backup codes are deleted
/// explicitly (the FK is ON DELETE CASCADE, but we delete codes first so the
/// operation is correct even if the FK direction is ever changed).
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure (transaction rolled back).
pub async fn disable(conn: &Client, user_id: uuid::Uuid) -> Result<bool> {
    conn.execute("BEGIN", &[])
        .await
        .map_err(|e| AuthError::Db(format!("totp disable begin: {e}")))?;
    let result = disable_tx(conn, user_id).await;
    match result {
        Ok(value) => {
            conn.execute("COMMIT", &[])
                .await
                .map_err(|e| AuthError::Db(format!("totp disable commit: {e}")))?;
            Ok(value)
        }
        Err(e) => {
            if let Err(rb) = conn.execute("ROLLBACK", &[]).await {
                tracing::error!(error = %rb, "totp disable rollback failed");
            }
            Err(e)
        }
    }
}

async fn disable_tx(conn: &Client, user_id: uuid::Uuid) -> Result<bool> {
    conn.execute(
        "DELETE FROM zeroship.totp_backup_codes WHERE user_id = $1",
        &[&user_id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("totp disable codes: {e}")))?;
    let n = conn
        .execute(
            "DELETE FROM zeroship.totp_credentials WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("totp disable credential: {e}")))?;
    Ok(n > 0)
}

/// List the user's UNUSED backup codes (for the login-time redeem scan).
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn unused_backup_codes(conn: &Client, user_id: uuid::Uuid) -> Result<Vec<BackupCode>> {
    let rows = conn
        .query(
            "SELECT id, code_hash, used_at \
             FROM zeroship.totp_backup_codes \
             WHERE user_id = $1 AND used_at IS NULL \
             ORDER BY id",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("totp unused codes: {e}")))?;
    Ok(rows
        .iter()
        .map(|row| BackupCode {
            id: row.get("id"),
            code_hash: row.get("code_hash"),
            used_at: row.try_get("used_at").ok().flatten(),
        })
        .collect())
}

/// Atomically mark a backup code used. Returns `true` only if the code was
/// still unused at the moment of the write (the single-use guard) — a
/// concurrent double-redeem loses the race and gets `false`.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn mark_backup_code_used(conn: &Client, code_id: i64) -> Result<bool> {
    let n = conn
        .execute(
            "UPDATE zeroship.totp_backup_codes \
             SET used_at = NOW() \
             WHERE id = $1 AND used_at IS NULL",
            &[&code_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("totp mark code used: {e}")))?;
    Ok(n > 0)
}
