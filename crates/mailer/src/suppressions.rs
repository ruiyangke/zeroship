//! `zeroship.email_suppressions` CRUD — the bounce/complaint blocklist that
//! every `Mailer` impl consults before transport.
//!
//! Schema (owned by Liquibase — `db/changelog/`):
//!
//! ```sql
//! CREATE TABLE zeroship.email_suppressions (
//!     email         CITEXT PRIMARY KEY,
//!     reason        TEXT NOT NULL,
//!     suppressed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
//!     provider_msg  TEXT
//! )
//! ```
//!
//! `CITEXT` makes lookups case-insensitive; we still pass `$1::citext`
//! explicitly because the driver binds `&str` as `text`, and `text =
//! citext` does not implicitly cast in PG (the operator only exists for
//! `citext = citext`).

use compio_postgres::Client;

use crate::types::MailerError;

/// Returns `true` if `email` is on the suppression list.
///
/// # Errors
///
/// [`MailerError::Transport`] on PG failure — a suppression-check failure is a
/// transport-layer fault from the caller's point of view (the message could not
/// be delivered).
pub async fn is_suppressed(conn: &Client, email: &str) -> Result<bool, MailerError> {
    let rows = conn
        .query(
            "SELECT 1 FROM zeroship.email_suppressions WHERE email = $1::citext",
            &[&email],
        )
        .await
        .map_err(|e| MailerError::Transport(format!("is_suppressed: {e}")))?;
    Ok(!rows.is_empty())
}

/// Add (or refresh) a suppression entry. Idempotent: re-suppressing the
/// same address overwrites `reason` + `provider_msg` (so a hard-bounce
/// followed by a complaint correctly lands on the latest reason).
///
/// # Errors
///
/// [`MailerError::Transport`] on PG failure.
pub async fn add(
    conn: &Client,
    email: &str,
    reason: &str,
    provider_msg: Option<&str>,
) -> Result<(), MailerError> {
    conn.execute(
        "INSERT INTO zeroship.email_suppressions (email, reason, provider_msg) \
         VALUES ($1::citext, $2, $3) \
         ON CONFLICT (email) DO UPDATE \
            SET reason = EXCLUDED.reason, \
                provider_msg = EXCLUDED.provider_msg",
        &[&email, &reason, &provider_msg],
    )
    .await
    .map_err(|e| MailerError::Transport(format!("suppressions::add: {e}")))?;
    Ok(())
}
