//! Hot-retention sweeper for `zeroship.audit_events`. Drops rows past their
//! event-class TTL. Cold-tiering to S3/etc. is post-launch.
//!
//! Buckets per proposal §15:
//!   - Security (365d hot): `login_*`, `oauth_*`, `magic_redeemed_*`,
//!     `verification_redeemed`, `password_changed`, `session_*`,
//!     `key_rotation_*`, `hydra_accept_*`
//!   - PII (90d): `signup`, `signup_blocked`, `verification_issued`,
//!     `password_reset_requested`, `magic_issued`, `oauth_start`
//!   - Debug (30d): `mailer_*`
//!   - Forever: `refresh_reuse_detected` (security alert signal —
//!     never swept)
//!
//! Events not in any bucket are kept forever (defensive default — better
//! to retain a not-yet-classified event than silently delete forensic
//! evidence).
//!
//! Companion: [`super::jwk_rotation`] rotates the hydra JWKS on its own
//! ticker (different cadence, different table — kept in their own
//! modules so a JWK-rotation incident never blocks log retention and
//! vice versa).

use std::sync::Arc;
use std::time::Duration;

use compio_postgres::Client;

use crate::config::AuthConfig;
use crate::error::{AuthError, Result};

/// Security-class events. 365-day hot retention — long enough for
/// forensic timelines on intrusion investigations.
const SECURITY: &[&str] = &[
    "login_success",
    "login_failure",
    "oauth_callback_success",
    "oauth_callback_failure",
    "oauth_link_success",
    "oauth_link_failed",
    "oauth_unlink_success",
    "oauth_unlink_refused",
    "magic_redeemed_same_device",
    "magic_redeemed_cross_device",
    "magic_complete",
    "verification_redeemed",
    "password_changed",
    "session_created",
    "session_rotated",
    "session_revoked",
    "key_rotation_announced",
    "key_rotation_completed",
    "hydra_accept_login",
    "hydra_accept_consent",
    "hydra_accept_logout",
    "hydra_delete_session",
];

/// PII-bearing events. 90-day hot retention — short enough to limit
/// data-protection surface, long enough to debug signup funnels.
const PII: &[&str] = &[
    "signup",
    "signup_blocked",
    "verification_issued",
    "password_reset_requested",
    "magic_issued",
    "oauth_start",
];

/// Debug-class events. 30-day hot retention — mailer plumbing only,
/// not security-relevant.
const DEBUG: &[&str] = &[
    "mailer_send",
    "mailer_bounce",
    "mailer_complaint",
    "mailer_suppressed",
];

/// Cron entry point. Loops forever; each iteration runs one [`tick`]
/// then sleeps `audit_retention_check_secs` (1 h default).
///
/// Errors inside a single tick are logged and swallowed so a transient
/// PG hiccup doesn't kill the cron task.
//
// `compio_postgres::Client` holds a connection handle that is `!Send`;
// the lint is structural, not actionable (mirrors `jwk_rotation::run`).
#[allow(clippy::future_not_send)]
pub async fn run(db: Arc<Client>, cfg: Arc<AuthConfig>) {
    let interval_secs = cfg.audit_retention_check_secs;
    tracing::info!(interval_secs, "audit_retention cron starting");
    loop {
        if let Err(e) = tick(&db).await {
            tracing::error!(error = %e, "audit_retention tick failed");
        }
        compio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

/// Run one retention sweep against all three buckets. Exposed (under
/// `#[doc(hidden)]`) so the live-PG integration test in
/// `tests/audit_retention_test.rs` can drive a single tick
/// deterministically without sitting on the cron sleep.
#[doc(hidden)]
pub async fn tick(db: &Client) -> Result<()> {
    // `zeroship.audit_events` is append-only — a BEFORE DELETE trigger rejects any
    // tampering. This retention sweep is the single sanctioned deleter, so it
    // flags the connection (`zeroship.audit_retention = 'on'`); the trigger
    // permits DELETEs only while that GUC is set. The flag is cleared afterward
    // (in all paths) so nothing else on this connection can delete. See the
    // `zeroship.audit_events_block_tamper()` trigger in db/changelog/0002_auth.sql.
    db.batch_execute("SET zeroship.audit_retention = 'on'")
        .await
        .map_err(|e| AuthError::Db(format!("audit retention: enable sweep: {e}")))?;

    let swept = sweep_all(db).await;

    // Always clear the flag, even if a delete failed mid-sweep.
    let _ = db.batch_execute("SET zeroship.audit_retention = 'off'").await;

    let (security_deleted, pii_deleted, debug_deleted) = swept?;
    let total = security_deleted + pii_deleted + debug_deleted;
    if total > 0 {
        tracing::info!(
            security_deleted,
            pii_deleted,
            debug_deleted,
            "audit_retention sweep completed"
        );
    }
    Ok(())
}

/// Sweep all three buckets. Split out so [`tick`] can bracket it with the
/// retention GUC and still guarantee the flag is cleared on the error path.
async fn sweep_all(db: &Client) -> Result<(u64, u64, u64)> {
    let security_deleted = delete_older_than(db, SECURITY, 365).await?;
    let pii_deleted = delete_older_than(db, PII, 90).await?;
    let debug_deleted = delete_older_than(db, DEBUG, 30).await?;
    Ok((security_deleted, pii_deleted, debug_deleted))
}

/// Delete `zeroship.audit_events` rows whose `event_type` is in `event_types`
/// and whose `occurred_at` is older than `days` days. Returns the row
/// count.
///
/// The interval is built as `($2::text || ' days')::interval` because
/// `PostgreSQL`'s interval-literal cast doesn't accept a parameterised
/// numeric on the LHS directly. Days is bound as text (rather than via a
/// `Vec<&str>` cast on the array) to sidestep `postgres-types`'s slice
/// quirks.
async fn delete_older_than(db: &Client, event_types: &[&str], days: i64) -> Result<u64> {
    // Bind the array as `Vec<&str>` so `postgres-types` resolves it to
    // `text[]` cleanly — bare `&[&str]` doesn't always coerce through
    // the `ToSql` impl matrix.
    let types_owned: Vec<&str> = event_types.to_vec();
    let days_str = days.to_string();
    let affected = db
        .execute(
            "DELETE FROM zeroship.audit_events \
             WHERE event_type = ANY($1) \
               AND occurred_at < NOW() - ($2::text || ' days')::interval",
            &[&types_owned, &days_str],
        )
        .await
        .map_err(|e| {
            AuthError::Db(format!(
                "audit retention sweep ({} events): {e}",
                event_types.len()
            ))
        })?;
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_lists_cover_expected_event_types() {
        // Smoke-check we haven't accidentally dropped a class.
        assert!(SECURITY.contains(&"login_success"));
        assert!(SECURITY.contains(&"password_changed"));
        assert!(PII.contains(&"signup"));
        assert!(PII.contains(&"magic_issued"));
        assert!(DEBUG.contains(&"mailer_send"));
    }

    #[test]
    fn refresh_reuse_is_not_swept() {
        // refresh_reuse_detected is intentionally NOT in any bucket —
        // it's the canary for refresh-token theft and must persist
        // indefinitely so SIEMs can correlate across long incidents.
        assert!(
            !SECURITY
                .iter()
                .chain(PII)
                .chain(DEBUG)
                .any(|s| *s == "refresh_reuse_detected"),
            "refresh_reuse_detected must never be in a sweep bucket"
        );
    }
}
