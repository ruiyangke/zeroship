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

use crate::config::AuthConfig;
use crate::error::{AuthError, Result};
use crate::op::refresh;

const INTERVAL_SECS: u64 = 60 * 60;

/// Idle window after which a `zeroship.rate_limits` row is reapable (SEC-3).
///
/// A leaky bucket is fully refilled — and therefore lossless to delete — once
/// it has been idle long enough to reach capacity; the slowest profile in
/// `ratelimit.rs` refills within an hour, so a 24h idle window is amply safe.
/// 24h also matches the relay `relay_seen:` dedup sentinel's own TTL (those
/// rows live in this same table), so one sweep correctly reaps both.
const RATE_LIMITS_GRACE_HOURS: i64 = 24;

/// Retention predicate backing the `zeroship.rate_limits` sweep: a row whose
/// last update is older than [`RATE_LIMITS_GRACE_HOURS`] is stale and reapable.
/// The cron's DELETE mirrors this exact boundary in SQL (it binds the same
/// constant as the interval). Test-only — the production reap is the SQL.
#[cfg(test)]
fn rate_limit_row_is_stale(idle_hours: i64) -> bool {
    idle_hours > RATE_LIMITS_GRACE_HOURS
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenSweepReport {
    pub magic_links_deleted: u64,
    pub password_resets_deleted: u64,
    pub magic_completions_deleted: u64,
    pub email_verifications_deleted: u64,
    pub token_revocations_deleted: u64,
    pub rate_limits_deleted: u64,
    pub refresh_tokens_deleted: u64,
    pub refresh_idem_reaped: u64,
}

impl TokenSweepReport {
    fn total(self) -> u64 {
        self.magic_links_deleted
            + self.password_resets_deleted
            + self.magic_completions_deleted
            + self.email_verifications_deleted
            + self.token_revocations_deleted
            + self.rate_limits_deleted
            + self.refresh_tokens_deleted
            + self.refresh_idem_reaped
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
pub async fn run(db: Arc<Client>, cfg: Arc<AuthConfig>) {
    tracing::info!(interval_secs = INTERVAL_SECS, "token_sweep cron starting");
    loop {
        match tick(&db, &cfg.db_url).await {
            Ok(report) => {
                tracing::info!(
                    magic_links_deleted = report.magic_links_deleted,
                    password_resets_deleted = report.password_resets_deleted,
                    magic_completions_deleted = report.magic_completions_deleted,
                    email_verifications_deleted = report.email_verifications_deleted,
                    token_revocations_deleted = report.token_revocations_deleted,
                    rate_limits_deleted = report.rate_limits_deleted,
                    refresh_tokens_deleted = report.refresh_tokens_deleted,
                    refresh_idem_reaped = report.refresh_idem_reaped,
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
pub async fn tick(db: &Client, refresh_db_url: &str) -> Result<TokenSweepReport> {
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
    let (refresh_tokens_deleted, refresh_idem_reaped) = refresh::sweep_refresh_tokens(refresh_db_url)
        .await
        .map_err(|e| AuthError::Db(format!("token_sweep zeroship.oauth_refresh_tokens: {e}")))?;

    // SEC-3: reap idle rate-limit buckets (and relay dedup sentinels, which
    // share this table) so a forged-IP flood can't leave permanent rows. The
    // window is the single-source-of-truth `RATE_LIMITS_GRACE_HOURS` (mirrored
    // by `rate_limit_row_is_stale`); we bind it as a parameter rather than
    // string-formatting an INTERVAL literal.
    let rate_limits_deleted = db
        .execute(
            "DELETE FROM zeroship.rate_limits \
             WHERE updated_at < NOW() - (make_interval(hours => $1::INT))",
            &[&i32::try_from(RATE_LIMITS_GRACE_HOURS).unwrap_or(24)],
        )
        .await
        .map_err(|e| AuthError::Db(format!("token_sweep zeroship.rate_limits: {e}")))?;

    Ok(TokenSweepReport {
        magic_links_deleted,
        password_resets_deleted,
        magic_completions_deleted,
        email_verifications_deleted,
        token_revocations_deleted,
        rate_limits_deleted,
        refresh_tokens_deleted,
        refresh_idem_reaped,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_retention_predicate_boundary() {
        // SEC-3: the rate-limit sweep reaps a bucket only once it is idle
        // STRICTLY longer than the 24h grace window. A bucket touched within
        // the window (or exactly at it) is kept; an older one is reaped. The
        // cron's `INTERVAL '24 hours'` DELETE mirrors this boundary.
        assert_eq!(RATE_LIMITS_GRACE_HOURS, 24);
        assert!(!rate_limit_row_is_stale(0), "a just-touched bucket is kept");
        assert!(!rate_limit_row_is_stale(1), "a recently-used bucket is kept");
        assert!(
            !rate_limit_row_is_stale(24),
            "a bucket exactly at the grace window is kept (boundary inclusive)"
        );
        assert!(
            rate_limit_row_is_stale(25),
            "a bucket idle past the grace window is reaped"
        );
        assert!(rate_limit_row_is_stale(48));
    }

    #[test]
    fn total_includes_rate_limits_deleted() {
        // The aggregate the cron logs must count reaped rate-limit rows.
        let report = TokenSweepReport {
            magic_links_deleted: 1,
            password_resets_deleted: 1,
            magic_completions_deleted: 1,
            email_verifications_deleted: 1,
            token_revocations_deleted: 1,
            rate_limits_deleted: 3,
            refresh_tokens_deleted: 2,
            refresh_idem_reaped: 4,
        };
        assert_eq!(report.total(), 14, "refresh counts must be summed in total()");
    }
}
