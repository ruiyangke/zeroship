//! `zeroship.identities` CRUD — OAuth/OIDC provider linkages keyed on (provider, subject).
//!
//! Each row represents one external identity (Google/GitHub/etc.) bound to a
//! local `zeroship.users` row. The `(provider, subject)` pair is `UNIQUE` —
//! attempting to link the same external identity twice surfaces PG SQLSTATE
//! `23505` (`unique_violation`) which the linker module (U4) translates into a
//! "already linked to another user" policy decision.

use compio_postgres::Client;
use uuid::Uuid;

use crate::error::{AuthError, Result};

#[derive(Debug, Clone)]
pub struct Identity {
    pub id: Uuid,
    pub user_id: Uuid,
    pub provider: String,
    pub subject: String,
    pub email_at_link: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedUnlink {
    Unlinked,
    NotLinked,
    WouldOrphan,
}

/// Find an identity by (provider, subject). None if not linked.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn find_by_provider_subject(
    conn: &Client,
    provider: &str,
    subject: &str,
) -> Result<Option<Identity>> {
    let rows = conn
        .query(
            "SELECT id, user_id, provider, subject, email_at_link::text \
             FROM zeroship.identities \
             WHERE provider = $1 AND subject = $2",
            &[&provider, &subject],
        )
        .await
        .map_err(|e| AuthError::Db(format!("identities find: {e}")))?;
    Ok(rows.first().map(row_to_identity))
}

/// Link an identity to an existing user.
///
/// `email_at_link` is the email the upstream provider reported at link time —
/// stored for audit (the user's `zeroship.users.email` is the source of truth).
/// `raw_profile` is the full provider profile JSON, kept opaque for future
/// re-extraction needs.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure. In particular, attempting to link
/// an identity that already exists (same `provider` + `subject`) raises a
/// `UNIQUE` violation (SQLSTATE `23505`) — callers in the linker should
/// inspect the error and apply policy (reject re-link to a different user).
pub async fn link(
    conn: &Client,
    user_id: Uuid,
    provider: &str,
    subject: &str,
    email_at_link: Option<&str>,
    raw_profile: Option<&serde_json::Value>,
) -> Result<Identity> {
    let rows = conn
        .query(
            "INSERT INTO zeroship.identities (user_id, provider, subject, email_at_link, raw_profile) \
             VALUES ($1, $2, $3, $4::citext, $5) \
             RETURNING id, user_id, provider, subject, email_at_link::text",
            &[&user_id, &provider, &subject, &email_at_link, &raw_profile],
        )
        .await
        .map_err(|e| AuthError::Db(format!("identities link: {e}")))?;
    let row = rows
        .first()
        .ok_or_else(|| AuthError::Db("identities link: empty return".into()))?;
    Ok(row_to_identity(row))
}

/// List a user's linked identities (for the /me page).
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn list_for_user(conn: &Client, user_id: Uuid) -> Result<Vec<Identity>> {
    let rows = conn
        .query(
            "SELECT id, user_id, provider, subject, email_at_link::text \
             FROM zeroship.identities WHERE user_id = $1 \
             ORDER BY linked_at",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("identities list: {e}")))?;
    Ok(rows.iter().map(row_to_identity).collect())
}

/// Unlink an identity. Returns `true` if a row was deleted, `false` if no
/// matching link existed.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn unlink(conn: &Client, user_id: Uuid, provider: &str) -> Result<bool> {
    let affected = conn
        .execute(
            "DELETE FROM zeroship.identities WHERE user_id = $1 AND provider = $2",
            &[&user_id, &provider],
        )
        .await
        .map_err(|e| AuthError::Db(format!("identities unlink: {e}")))?;
    Ok(affected > 0)
}

/// Unlink an identity only if the user keeps at least one sign-in method.
///
/// Concurrent unlinks against *different* providers on a no-password,
/// two-identity user must not both succeed (that would orphan the account).
/// A single auto-commit statement can't guarantee this under READ COMMITTED:
/// the orphan-check read of `zeroship.identities` uses the statement's snapshot,
/// which is taken *before* it blocks on `zeroship.users FOR UPDATE`, so the loser
/// still sees the winner's not-yet-deleted identity and deletes its own too.
///
/// We therefore serialize the two callers with a per-user *session* advisory
/// lock (`with_advisory_lock`): the loser's unlink statement runs as a fresh
/// implicit transaction only after the winner has committed and released the
/// lock, so its orphan check observes the committed delete and correctly
/// refuses (`WouldOrphan`). The in-statement `FOR UPDATE` + orphan guard stay
/// as belt-and-braces.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn unlink_preserving_credential(
    conn: &Client,
    user_id: Uuid,
    provider: &str,
) -> Result<GuardedUnlink> {
    let lock_key = crate::advisory_lock::identity_unlink_lock_key(&user_id);
    crate::advisory_lock::with_advisory_lock(conn, lock_key, || async {
        unlink_preserving_credential_locked(conn, user_id, provider).await
    })
    .await
}

async fn unlink_preserving_credential_locked(
    conn: &Client,
    user_id: Uuid,
    provider: &str,
) -> Result<GuardedUnlink> {
    let row = conn
        .query_one(
            "WITH locked_user AS MATERIALIZED ( \
                 SELECT password_hash IS NOT NULL AS has_password \
                 FROM zeroship.users \
                 WHERE id = $1 \
                 FOR UPDATE \
             ), \
             target AS MATERIALIZED ( \
                 SELECT EXISTS ( \
                     SELECT 1 \
                     FROM zeroship.identities i \
                     WHERE i.user_id = $1 \
                       AND i.provider = $2 \
                 ) AS had_target \
                 FROM locked_user \
             ), \
             deleted AS ( \
                 DELETE FROM zeroship.identities i \
                 USING locked_user u, target t \
                 WHERE i.user_id = $1 \
                   AND i.provider = $2 \
                   AND t.had_target \
                   AND ( \
                       u.has_password \
                       OR EXISTS ( \
                           SELECT 1 \
                           FROM zeroship.identities other \
                           WHERE other.user_id = $1 \
                             AND other.provider <> $2 \
                       ) \
                   ) \
                 RETURNING i.provider \
             ) \
             SELECT \
                 EXISTS (SELECT 1 FROM deleted) AS deleted, \
                 EXISTS ( \
                     SELECT 1 \
                     FROM target t \
                     WHERE t.had_target \
                 ) AS had_target",
            &[&user_id, &provider],
        )
        .await
        .map_err(|e| AuthError::Db(format!("identities guarded unlink: {e}")))?;

    let deleted: bool = row.get("deleted");
    let had_target: bool = row.get("had_target");
    Ok(match (deleted, had_target) {
        (true, _) => GuardedUnlink::Unlinked,
        (false, true) => GuardedUnlink::WouldOrphan,
        (false, false) => GuardedUnlink::NotLinked,
    })
}

fn row_to_identity(row: &compio_postgres::Row) -> Identity {
    Identity {
        id: row.get("id"),
        user_id: row.get("user_id"),
        provider: row.get("provider"),
        subject: row.get("subject"),
        email_at_link: row.try_get::<_, String>("email_at_link").ok(),
    }
}
