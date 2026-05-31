//! Expired one-shot-token sweeper.
//!
//! Magic links, cross-device completions, email verifications, and reset
//! tokens are rejected at redemption time once expired or consumed. This
//! cron removes those stale rows after a 7-day grace window so failed
//! redemption diagnostics remain possible briefly without letting token
//! tables grow forever.

use std::sync::Arc;
use std::time::Duration;

use compio_postgres::Client;

use crate::error::{AuthError, Result};

const INTERVAL_SECS: u64 = 60 * 60;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenSweepReport {
    pub magic_links_deleted: u64,
    pub password_resets_deleted: u64,
    pub magic_completions_deleted: u64,
    pub email_verifications_deleted: u64,
    pub token_revocations_deleted: u64,
}

impl TokenSweepReport {
    fn total(self) -> u64 {
        self.magic_links_deleted
            + self.password_resets_deleted
            + self.magic_completions_deleted
            + self.email_verifications_deleted
            + self.token_revocations_deleted
    }
}

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then
/// sleeps one hour.
///
/// Errors inside a single tick are logged and swallowed so a transient PG
/// hiccup doesn't kill the cron task.
//
// `compio_postgres::Client` holds a connection handle that is `!Send`;
// the lint is structural, not actionable (mirrors the other cron tasks).
#[allow(clippy::future_not_send)]
pub async fn run(db: Arc<Client>) {
    tracing::info!(interval_secs = INTERVAL_SECS, "token_sweep cron starting");
    loop {
        match tick(&db).await {
            Ok(report) => {
                tracing::info!(
                    magic_links_deleted = report.magic_links_deleted,
                    password_resets_deleted = report.password_resets_deleted,
                    magic_completions_deleted = report.magic_completions_deleted,
                    email_verifications_deleted = report.email_verifications_deleted,
                    token_revocations_deleted = report.token_revocations_deleted,
                    total_deleted = report.total(),
                    "token_sweep completed"
                );
            }
            Err(e) => tracing::error!(error = %e, "token_sweep tick failed"),
        }
        compio::time::sleep(Duration::from_secs(INTERVAL_SECS)).await;
    }
}

/// Run one token sweep. Rows are eligible once either their expiry or
/// consumption timestamp is older than the 7-day grace window.
#[doc(hidden)]
pub async fn tick(db: &Client) -> Result<TokenSweepReport> {
    // Build the report in the initializer (every field is computed once here),
    // so there is no redundant `Default::default()` to reassign over.
    let password_resets_deleted = delete_magic_links(db, Some("reset")).await?;
    let magic_links_deleted = delete_magic_links(db, None).await?;
    let magic_completions_deleted = delete_table(
        db,
        "zeroship.magic_completions",
        "DELETE FROM zeroship.magic_completions \
         WHERE expires_at < NOW() - INTERVAL '7 days' \
            OR consumed_at < NOW() - INTERVAL '7 days'",
    )
    .await?;
    let email_verifications_deleted = delete_table(
        db,
        "zeroship.email_verifications",
        "DELETE FROM zeroship.email_verifications \
         WHERE expires_at < NOW() - INTERVAL '7 days' \
            OR consumed_at < NOW() - INTERVAL '7 days'",
    )
    .await?;
    let token_revocations_deleted =
        zeroship_core::wrapper_revocation::sweep_expired_families(db)
            .await
            .map_err(|e| AuthError::Db(format!("token_sweep zeroship.token_revocations: {e}")))?;

    Ok(TokenSweepReport {
        magic_links_deleted,
        password_resets_deleted,
        magic_completions_deleted,
        email_verifications_deleted,
        token_revocations_deleted,
    })
}

async fn delete_magic_links(db: &Client, purpose: Option<&str>) -> Result<u64> {
    match purpose {
        Some(purpose) => {
            db.execute(
                "DELETE FROM zeroship.magic_links \
                 WHERE purpose = $1 \
                   AND (expires_at < NOW() - INTERVAL '7 days' \
                        OR consumed_at < NOW() - INTERVAL '7 days')",
                &[&purpose],
            )
            .await
            .map_err(|e| AuthError::Db(format!("token_sweep zeroship.magic_links[{purpose}]: {e}")))
        }
        None => {
            db.execute(
                "DELETE FROM zeroship.magic_links \
                 WHERE purpose <> 'reset' \
                   AND (expires_at < NOW() - INTERVAL '7 days' \
                        OR consumed_at < NOW() - INTERVAL '7 days')",
                &[],
            )
            .await
            .map_err(|e| AuthError::Db(format!("token_sweep zeroship.magic_links: {e}")))
        }
    }
}

async fn delete_table(db: &Client, table: &str, sql: &str) -> Result<u64> {
    db.execute(sql, &[])
        .await
        .map_err(|e| AuthError::Db(format!("token_sweep {table}: {e}")))
}
