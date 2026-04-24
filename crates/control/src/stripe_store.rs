//! Stripe Connect ledger + creator-account linkage.
//!
//! Two tables (migrations in `registry.rs`):
//!
//! - `creator_accounts(creator_id UUID PK, stripe_account_id TEXT, onboarded_at)`
//!   One row per creator once they finish Stripe onboarding.
//!
//! - `payouts(id, creator_id FK, event_id UNIQUE, event_type, gross_amount,
//!    platform_fee, net_amount, currency, occurred_at, created_at)`
//!   One row per Stripe webhook event that moves money. `event_id` is
//!   Stripe's `evt_...` — the UNIQUE constraint makes retries idempotent.
//!
//! Amounts are integer minor-units (cents for USD, etc.) matching Stripe's wire.

use uuid::Uuid;

use crate::registry::Registry;

#[derive(Debug)]
pub enum StripeError {
    Db(String),
    Duplicate,
    NotFound,
    /// Callsite-level validation failure (bad shape, invalid amount, …).
    /// Maps to HTTP 400 — distinct from internal `Db` errors.
    Validation(String),
}

impl std::fmt::Display for StripeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(m) => write!(f, "{m}"),
            Self::Duplicate => write!(f, "event already recorded"),
            Self::NotFound => write!(f, "creator not linked"),
            Self::Validation(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for StripeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatorAccount {
    pub creator_id: Uuid,
    pub stripe_account_id: String,
    pub onboarded_at: String, // RFC3339 — callers can parse as needed
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutRecord {
    pub id: Uuid,
    pub creator_id: Uuid,
    pub event_id: String,
    pub event_type: String,
    pub gross_amount: i64,
    pub platform_fee: i64,
    pub net_amount: i64,
    pub currency: String,
    pub occurred_at: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub gross: i64,
    pub fee: i64,
    pub net: i64,
}

#[allow(missing_debug_implementations)]
pub struct StripeStore {
    registry: Registry,
}

impl StripeStore {
    pub fn new(registry: Registry) -> Self { Self { registry } }

    // ------------------------------------------------------------------
    // Creator ↔ Stripe account linkage
    // ------------------------------------------------------------------

    pub async fn link_account(
        &self,
        creator_id: Uuid,
        stripe_account_id: &str,
    ) -> Result<(), StripeError> {
        if !is_valid_stripe_account_id(stripe_account_id) {
            return Err(StripeError::Validation(format!(
                "stripe_account_id must match /^acct_[A-Za-z0-9]{{12,64}}$/, got '{}'",
                // Sanitize to guard against log-line injection.
                sanitize_for_display(stripe_account_id),
            )));
        }
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        conn.execute(
            "INSERT INTO creator_accounts(creator_id, stripe_account_id) VALUES($1, $2)
             ON CONFLICT (creator_id) DO UPDATE
                SET stripe_account_id = EXCLUDED.stripe_account_id,
                    onboarded_at = NOW()",
            &[&creator_id, &stripe_account_id],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(())
    }

    pub async fn unlink_account(&self, creator_id: Uuid) -> Result<bool, StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        let n = conn
            .execute(
                "DELETE FROM creator_accounts WHERE creator_id = $1",
                &[&creator_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(n > 0)
    }

    pub async fn get_account(&self, creator_id: Uuid) -> Result<Option<CreatorAccount>, StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT creator_id, stripe_account_id, onboarded_at::text
                 FROM creator_accounts WHERE creator_id = $1",
                &[&creator_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(rows.first().map(|r| CreatorAccount {
            creator_id: r.get("creator_id"),
            stripe_account_id: r.get("stripe_account_id"),
            onboarded_at: r.get("onboarded_at"),
        }))
    }

    // ------------------------------------------------------------------
    // Payout ledger
    // ------------------------------------------------------------------

    /// Record one ledger event. Returns `Err(Duplicate)` if `event_id`
    /// was already recorded (Stripe retries deliveries — we dedupe here
    /// so retry storms don't double-count revenue).
    ///
    /// `occurred_at_unix` is seconds since epoch (matches Stripe's
    /// `event.created` wire). Postgres `to_timestamp` converts it to
    /// TIMESTAMPTZ inside the INSERT.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_payout(
        &self,
        creator_id: Uuid,
        event_id: &str,
        event_type: &str,
        gross_amount: i64,
        platform_fee: i64,
        currency: &str,
        occurred_at_unix: i64,
    ) -> Result<PayoutRecord, StripeError> {
        // Stripe guarantees non-negative amounts on its wire; defense
        // in depth — a compromised webhook or malformed upstream could
        // otherwise corrupt the ledger with negative totals.
        if gross_amount < 0 || platform_fee < 0 {
            return Err(StripeError::Validation(
                "gross_amount and platform_fee must be non-negative".into(),
            ));
        }
        if platform_fee > gross_amount {
            return Err(StripeError::Validation(
                "platform_fee cannot exceed gross_amount".into(),
            ));
        }
        let net = gross_amount - platform_fee;
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "INSERT INTO payouts(creator_id, event_id, event_type, gross_amount, platform_fee, net_amount, currency, occurred_at)
                 VALUES($1, $2, $3, $4, $5, $6, $7, to_timestamp($8::double precision))
                 ON CONFLICT (event_id) DO NOTHING
                 RETURNING id, occurred_at::text AS occurred_at_text",
                &[&creator_id, &event_id, &event_type, &gross_amount, &platform_fee, &net, &currency, &(occurred_at_unix as f64)],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        let row = rows.first().ok_or(StripeError::Duplicate)?;
        Ok(PayoutRecord {
            id: row.get("id"),
            creator_id,
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            gross_amount,
            platform_fee,
            net_amount: net,
            currency: currency.to_string(),
            occurred_at: row.get("occurred_at_text"),
        })
    }

    /// Aggregate totals (sum over `payouts`).
    pub async fn total_earnings(&self, creator_id: Uuid) -> Result<Totals, StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        // SUM(BIGINT) returns NUMERIC in Postgres — cast back to BIGINT
        // so compio-postgres can decode it as i64.
        let rows = conn
            .query(
                "SELECT
                    COALESCE(SUM(gross_amount), 0)::BIGINT AS gross,
                    COALESCE(SUM(platform_fee), 0)::BIGINT AS fee,
                    COALESCE(SUM(net_amount), 0)::BIGINT AS net
                 FROM payouts WHERE creator_id = $1",
                &[&creator_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        let r = rows.first().ok_or_else(|| StripeError::Db("SUM returned no rows".into()))?;
        Ok(Totals {
            gross: r.get("gross"),
            fee: r.get("fee"),
            net: r.get("net"),
        })
    }

    /// Recent ledger events, newest first, capped at `limit`.
    pub async fn recent_payouts(
        &self,
        creator_id: Uuid,
        limit: i64,
    ) -> Result<Vec<PayoutRecord>, StripeError> {
        let limit = limit.clamp(1, 500);
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT id, creator_id, event_id, event_type, gross_amount, platform_fee, net_amount, currency, occurred_at::text
                 FROM payouts
                 WHERE creator_id = $1
                 ORDER BY occurred_at DESC
                 LIMIT $2",
                &[&creator_id, &limit],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(rows
            .iter()
            .map(|r| PayoutRecord {
                id: r.get("id"),
                creator_id: r.get("creator_id"),
                event_id: r.get("event_id"),
                event_type: r.get("event_type"),
                gross_amount: r.get("gross_amount"),
                platform_fee: r.get("platform_fee"),
                net_amount: r.get("net_amount"),
                currency: r.get("currency"),
                occurred_at: r.get("occurred_at"),
            })
            .collect())
    }
}

/// Stripe account IDs are `acct_` + 12–64 alphanumerics. `starts_with`
/// alone accepts `acct_; DROP TABLE payouts;--` which is parameterized
/// (safe from SQL injection) but still smells — tighten to the shape
/// Stripe actually issues.
pub(crate) fn is_valid_stripe_account_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("acct_") else { return false; };
    let bytes = rest.as_bytes();
    (12..=64).contains(&bytes.len())
        && bytes.iter().all(|b| b.is_ascii_alphanumeric())
}

/// Replace non-printable / non-ASCII chars with `?` so error messages
/// (and the logs they land in) can't be polluted with CRLF injection.
pub(crate) fn sanitize_for_display(s: &str) -> String {
    s.chars()
        .take(80)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    #[test]
    fn accepts_real_account_shapes() {
        assert!(is_valid_stripe_account_id("acct_1NfZo0Cz2XrwDw8A"));
        assert!(is_valid_stripe_account_id("acct_abcDEF123456"));
    }

    #[test]
    fn rejects_bogus_shapes() {
        assert!(!is_valid_stripe_account_id(""));
        assert!(!is_valid_stripe_account_id("cus_prefix_wrong"));
        assert!(!is_valid_stripe_account_id("acct_"));
        assert!(!is_valid_stripe_account_id("acct_short"));
        assert!(!is_valid_stripe_account_id("acct_; DROP TABLE payouts;--"));
        // 65 alphanumerics (one past the 64-char cap)
        assert!(!is_valid_stripe_account_id(&format!("acct_{}", "a".repeat(65))));
        assert!(!is_valid_stripe_account_id("acct_has-dash-chars"));
    }

    #[test]
    fn sanitize_strips_crlf() {
        assert_eq!(sanitize_for_display("normal"), "normal");
        assert_eq!(sanitize_for_display("line1\nline2"), "line1?line2");
        assert_eq!(sanitize_for_display("crlf\r\n"), "crlf??");
        // Truncation at 80 chars.
        let long: String = "a".repeat(200);
        assert_eq!(sanitize_for_display(&long).len(), 80);
    }
}
