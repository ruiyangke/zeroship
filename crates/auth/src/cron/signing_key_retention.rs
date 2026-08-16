//! Terminal retirement for rotated OIDC signing keys.
//!
//! A process can retain an old private key after another process moves its
//! registry row to `retiring`. Issuance therefore persists the greatest exact
//! token expiry on that row. This cron waits for that expiry plus the complete
//! published JWKS cache window and the OP clock-skew budget before changing the
//! row to terminal `retired`. The audit row survives, while the JWKS query no
//! longer selects it.

use std::sync::Arc;
use std::time::Duration;

use compio_postgres::Client;
use zeroship_core::device_grant::PLATFORM_TOKEN_MAX_TTL_SECS;

use crate::error::{AuthError, Result};
use crate::oidc::metadata::{JWKS_MAX_AGE_SECS, JWKS_STALE_WHILE_REVALIDATE_SECS};

const INTERVAL_SECS: u64 = 60 * 60;

/// The OP design's explicit allowance for issuer/verifier clock skew.
pub const OIDC_CLOCK_SKEW_ALLOWANCE_SECS: i64 = 2 * 60;

/// Time retained after the greatest exact token expiry on a key.
pub const RETENTION_AFTER_EXPIRY_SECS: i64 =
    JWKS_MAX_AGE_SECS + JWKS_STALE_WHILE_REVALIDATE_SECS + OIDC_CLOCK_SKEW_ALLOWANCE_SECS;

/// Full horizon from the last possible issue time for a maximum-lifetime token.
pub const RETENTION_HORIZON_SECS: i64 = PLATFORM_TOKEN_MAX_TTL_SECS + RETENTION_AFTER_EXPIRY_SECS;

/// Structured reason attached to every terminal-retirement log record.
pub const RETIREMENT_REASON: &str = "maximum issued expiry plus JWKS cache and clock skew elapsed";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredSigningKey {
    pub kid: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SigningKeyRetentionReport {
    pub retired: Vec<RetiredSigningKey>,
}

/// Cron entry point. Runs one tick immediately, then sleeps for one hour.
#[allow(clippy::future_not_send)]
pub async fn run(db: Arc<Client>) {
    tracing::info!(
        interval_secs = INTERVAL_SECS,
        retention_horizon_secs = RETENTION_HORIZON_SECS,
        "signing_key_retention cron starting"
    );
    loop {
        match tick(&db).await {
            Ok(report) => tracing::info!(
                retired = report.retired.len(),
                "signing_key_retention completed"
            ),
            Err(err) => tracing::error!(error = %err, "signing_key_retention tick failed"),
        }
        compio::time::sleep(Duration::from_secs(INTERVAL_SECS)).await;
    }
}

/// Retire every due key with one atomic, concurrency-safe statement.
///
/// Issuance updates `max_issued_expires_at` on the same row before releasing a
/// token. PostgreSQL row-update serialization makes the race safe: an issuance
/// update that wins extends the predicate, while a retirement that wins changes
/// the status so the issuance update affects zero rows and discards its token.
#[doc(hidden)]
pub async fn tick(db: &Client) -> Result<SigningKeyRetentionReport> {
    let rows = db
        .query(
            "UPDATE zeroship.signing_keys \
             SET status = 'retired', retired_at = COALESCE(retired_at, NOW()) \
             WHERE status = 'retiring' \
               AND retiring_at IS NOT NULL \
               AND COALESCE( \
                    max_issued_expires_at, \
                    retiring_at + ($1::BIGINT * INTERVAL '1 second') \
               ) < NOW() - ($2::BIGINT * INTERVAL '1 second') \
             RETURNING kid",
            &[&PLATFORM_TOKEN_MAX_TTL_SECS, &RETENTION_AFTER_EXPIRY_SECS],
        )
        .await
        .map_err(|err| AuthError::Db(format!("signing_key_retention signing_keys: {err}")))?;

    let retired = rows
        .into_iter()
        .map(|row| RetiredSigningKey {
            kid: row.get("kid"),
            reason: RETIREMENT_REASON,
        })
        .collect::<Vec<_>>();
    for key in &retired {
        tracing::info!(
            kid = %key.kid,
            reason = key.reason,
            retention_after_expiry_secs = RETENTION_AFTER_EXPIRY_SECS,
            fallback_retention_horizon_secs = RETENTION_HORIZON_SECS,
            "OIDC signing key retired"
        );
    }

    Ok(SigningKeyRetentionReport { retired })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retirement_is_due(
        status: &str,
        max_issued_expiry: Option<i64>,
        retiring_at: Option<i64>,
        now: i64,
    ) -> bool {
        if status != "retiring" {
            return false;
        }
        let Some(expiry) = max_issued_expiry.or_else(|| {
            retiring_at.map(|retiring| retiring.saturating_add(PLATFORM_TOKEN_MAX_TTL_SECS))
        }) else {
            return false;
        };
        expiry.saturating_add(RETENTION_AFTER_EXPIRY_SECS) < now
    }

    #[test]
    fn horizon_includes_maximum_lifetime_full_jwks_cache_and_clock_skew() {
        assert_eq!(PLATFORM_TOKEN_MAX_TTL_SECS, 43_200);
        assert_eq!(RETENTION_AFTER_EXPIRY_SECS, 720);
        assert_eq!(RETENTION_HORIZON_SECS, 43_920);
    }

    #[test]
    fn retiring_key_inside_or_at_horizon_is_kept() {
        let now = 100_000;
        let at_boundary = now - RETENTION_AFTER_EXPIRY_SECS;
        assert!(!retirement_is_due(
            "retiring",
            Some(at_boundary + 1),
            Some(1),
            now
        ));
        assert!(!retirement_is_due(
            "retiring",
            Some(at_boundary),
            Some(1),
            now
        ));
        assert!(!retirement_is_due(
            "retiring",
            None,
            Some(now - RETENTION_HORIZON_SECS),
            now
        ));
    }

    #[test]
    fn retiring_key_past_horizon_is_due() {
        let now = 100_000;
        assert!(retirement_is_due(
            "retiring",
            Some(now - RETENTION_AFTER_EXPIRY_SECS - 1),
            Some(1),
            now
        ));
        assert!(retirement_is_due(
            "retiring",
            None,
            Some(now - RETENTION_HORIZON_SECS - 1),
            now
        ));
    }

    #[test]
    fn active_next_and_retired_keys_are_never_due() {
        for status in ["active", "next", "retired"] {
            assert!(!retirement_is_due(status, Some(1), Some(1), 100_000));
        }
    }

    #[test]
    fn retirement_reason_names_every_elapsed_allowance() {
        assert!(RETIREMENT_REASON.contains("issued expiry"));
        assert!(RETIREMENT_REASON.contains("JWKS cache"));
        assert!(RETIREMENT_REASON.contains("clock skew"));
    }
}
