//! Shared wrapper-token subject revocation helpers.
//!
//! Back-channel logout records the UUID subject that signed out. Gateway
//! wrapper verification then rejects wrappers whose `iat` is at or before
//! the recorded revocation time.

use compio_postgres::{Client, Error};
use uuid::Uuid;

pub const WRAPPER_REVOCATION_RETENTION_HOURS: i32 = 24;

#[must_use]
pub fn subject_uuid(sub: &str) -> Option<Uuid> {
    Uuid::parse_str(sub).ok()
}

pub async fn revoke_subject(db: &Client, subject: Uuid) -> Result<u64, Error> {
    db.execute(
        "INSERT INTO auth.wrapper_revoked_subjects (subject, revoked_at) \
         VALUES ($1, NOW()) \
         ON CONFLICT (subject) DO UPDATE SET revoked_at = EXCLUDED.revoked_at",
        &[&subject],
    )
    .await
}

pub async fn is_subject_revoked_since(
    db: &Client,
    subject: Uuid,
    iat: i64,
) -> Result<bool, Error> {
    let row = db
        .query_one(
            "SELECT EXISTS ( \
                SELECT 1 FROM auth.wrapper_revoked_subjects \
                WHERE subject = $1 \
                  AND revoked_at >= to_timestamp($2::double precision) \
             ) AS revoked",
            &[&subject, &iat],
        )
        .await?;
    Ok(row.get("revoked"))
}

pub async fn sweep_expired_subjects(db: &Client) -> Result<u64, Error> {
    db.execute(
        "DELETE FROM auth.wrapper_revoked_subjects \
         WHERE revoked_at < NOW() - make_interval(hours => $1)",
        &[&WRAPPER_REVOCATION_RETENTION_HOURS],
    )
    .await
}
