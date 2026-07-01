//! Shared account-state checks before minting or extending login authority.

use compio_postgres::Client;
use thiserror::Error;

use crate::error::AuthError;

#[derive(Debug, Error)]
pub enum LoginIneligible {
    #[error("account temporarily locked")]
    Locked,
    #[error("account disabled")]
    Disabled,
    #[error("user not found")]
    NotFound,
    #[error("{0}")]
    Store(#[from] AuthError),
}

impl LoginIneligible {
    #[must_use]
    pub const fn is_account_state(&self) -> bool {
        matches!(
            self,
            Self::Locked | Self::Disabled | Self::NotFound
        )
    }
}

/// Verify a local user is allowed to receive login authority.
///
/// `Ok(())` means the user exists, is not disabled, and is not currently
/// locked. Call this immediately before minting or extending login authority.
pub async fn check_user_eligible(
    conn: &Client,
    user_id: uuid::Uuid,
) -> std::result::Result<(), LoginIneligible> {
    let rows = conn
        .query(
            "SELECT locked_until, disabled_at \
             FROM zeroship.users \
             WHERE id = $1",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("eligibility check_user_eligible: {e}")))?;
    let Some(row) = rows.first() else {
        return Err(LoginIneligible::NotFound);
    };

    let disabled_at: Option<chrono::DateTime<chrono::Utc>> =
        row.try_get("disabled_at").ok();
    if disabled_at.is_some() {
        return Err(LoginIneligible::Disabled);
    }

    let locked_until: Option<chrono::DateTime<chrono::Utc>> =
        row.try_get("locked_until").ok();
    if locked_until.is_some_and(|t| t > chrono::Utc::now()) {
        return Err(LoginIneligible::Locked);
    }

    Ok(())
}
