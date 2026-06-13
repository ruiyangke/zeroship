//! Stripe Connect ledger + creator-account linkage.
//!
//! Two tables (migrations in `registry.rs`):
//!
//! - `control.creator_accounts(creator_id UUID PK, stripe_account_id TEXT, onboarded_at)`
//!   One row per creator once they finish Stripe onboarding.
//!
//! - `control.payouts(id, creator_id FK, event_id UNIQUE, event_type, gross_amount,
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
    /// A non-2xx response from the Stripe REST API (billing PR6). `code` is the
    /// machine-readable `error.code` from Stripe's JSON body when present.
    Api { status: u16, code: Option<String> },
}

impl std::fmt::Display for StripeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(m) => write!(f, "{m}"),
            Self::Duplicate => write!(f, "event already recorded"),
            Self::NotFound => write!(f, "creator not linked"),
            Self::Validation(m) => write!(f, "{m}"),
            Self::Api { status, code } => match code {
                Some(c) => write!(f, "stripe API error {status} ({c})"),
                None => write!(f, "stripe API error {status}"),
            },
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
pub struct AccountHistoryRow {
    pub id: Uuid,
    pub stripe_account_id: String,
    pub linked_at: String,
    pub unlinked_at: Option<String>,
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

        // Wrap the live-row check and history mutation in one transaction.
        // Existing links are read FOR UPDATE so concurrent relinks for the
        // same creator serialize before closing/opening history spans.
        conn.execute("BEGIN", &[])
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        let txn_result = link_account_txn(&conn, creator_id, stripe_account_id).await;
        match &txn_result {
            Ok(()) => {
                conn.execute("COMMIT", &[])
                    .await
                    .map_err(|e| StripeError::Db(e.to_string()))?;
            }
            Err(_) => {
                // Best-effort ROLLBACK — if it fails the conn is dead
                // anyway and Postgres will roll back on disconnect.
                let _ = conn.execute("ROLLBACK", &[]).await;
            }
        }
        txn_result
    }

    /// Soft-delete: mark `unlinked_at = NOW()`. Preserves the row +
    /// every payouts FK pointing at it. `link_account` later re-opens
    /// the row by clearing `unlinked_at`. Wrapped in a transaction so
    /// the live-table close + history close happen atomically.
    pub async fn unlink_account(&self, creator_id: Uuid) -> Result<bool, StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        conn.execute("BEGIN", &[])
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        let result = unlink_account_txn(&conn, creator_id).await;
        match &result {
            Ok(_) => {
                conn.execute("COMMIT", &[])
                    .await
                    .map_err(|e| StripeError::Db(e.to_string()))?;
            }
            Err(_) => {
                let _ = conn.execute("ROLLBACK", &[]).await;
            }
        }
        result
    }

    pub async fn get_account(&self, creator_id: Uuid) -> Result<Option<CreatorAccount>, StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                // Return None for a soft-deleted creator — caller treats
                // the link as gone. History is still queryable via
                // get_account_history.
                "SELECT creator_id, stripe_account_id,
                    to_char(onboarded_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS onboarded_at
                 FROM zeroship.creator_accounts
                 WHERE creator_id = $1 AND unlinked_at IS NULL",
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

    /// Return the link-history rows for a creator (newest first).
    /// Each row spans `[linked_at, unlinked_at)` for a single
    /// stripe_account_id binding.
    pub async fn account_history(
        &self,
        creator_id: Uuid,
    ) -> Result<Vec<AccountHistoryRow>, StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT id, stripe_account_id,
                    to_char(linked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS linked_at_text,
                    CASE WHEN unlinked_at IS NULL THEN NULL
                         ELSE to_char(unlinked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
                    END AS unlinked_at_text
                 FROM zeroship.creator_account_history
                 WHERE creator_id = $1
                 ORDER BY linked_at DESC",
                &[&creator_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(rows.iter().map(|r| AccountHistoryRow {
            id: r.get("id"),
            stripe_account_id: r.get("stripe_account_id"),
            linked_at: r.get("linked_at_text"),
            unlinked_at: r.get("unlinked_at_text"),
        }).collect())
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
        payload_hash: Option<&[u8]>,
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
        // `to_char(... 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')` returns an
        // RFC3339 UTC string regardless of the session's TimeZone.
        // `occurred_at::text` was timezone-dependent.
        let rows = conn
            .query(
                "INSERT INTO zeroship.payouts(creator_id, event_id, event_type, gross_amount, platform_fee, net_amount, currency, occurred_at, payload_hash)
                 VALUES($1, $2, $3, $4, $5, $6, $7, to_timestamp($8::double precision), $9)
                 ON CONFLICT (event_id) DO NOTHING
                 RETURNING id,
                   to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS occurred_at_text",
                &[&creator_id, &event_id, &event_type, &gross_amount, &platform_fee, &net, &currency, &(occurred_at_unix as f64), &payload_hash.map(|b| b.to_vec())],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        let row = match rows.first() {
            Some(r) => r,
            None => {
                // Row already exists. Verify payload matches so upstream
                // tampering surfaces as a loud error rather than a silent
                // "duplicate".
                if let Some(new_hash) = payload_hash {
                    let stored = conn
                        .query(
                            "SELECT payload_hash FROM zeroship.payouts WHERE event_id = $1",
                            &[&event_id],
                        )
                        .await
                        .map_err(|e| StripeError::Db(e.to_string()))?;
                    if let Some(r) = stored.first() {
                        let stored_hash: Option<Vec<u8>> = r.get("payload_hash");
                        if let Some(h) = stored_hash {
                            if h != new_hash {
                                tracing::warn!(
                                    event_id = %sanitize_for_display(event_id),
                                    "stripe: payload_hash mismatch — possible replay/tamper"
                                );
                                return Err(StripeError::Validation(
                                    "duplicate event_id with mismatched payload".into(),
                                ));
                            }
                        }
                    }
                }
                return Err(StripeError::Duplicate);
            }
        };
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

    /// Aggregate totals (sum over `control.payouts`).
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
                 FROM zeroship.payouts WHERE creator_id = $1",
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
                "SELECT id, creator_id, event_id, event_type, gross_amount, platform_fee, net_amount, currency,
                    to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS occurred_at
                 FROM zeroship.payouts
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

    // ------------------------------------------------------------------
    // Creator billing identity — the Stream-1 platform Customer (cus_…).
    //
    // Distinct from the Stream-2 Connect `acct_…` above: this is the
    // PLATFORM-side Customer the infra-cost reconciler invoices. Keyed by
    // creator_id (a user id), one row per creator, created lazily on first
    // `billing/setup`. (billing PR6, changeset 0040.)
    // ------------------------------------------------------------------

    /// The creator's platform Stripe Customer id (`cus_…`), or `None` if no row
    /// exists yet or the row has no customer id.
    pub async fn get_customer(&self, creator_id: Uuid) -> Result<Option<String>, StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT stripe_customer_id FROM zeroship.creator_billing WHERE creator_id = $1",
                &[&creator_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(rows
            .first()
            .and_then(|r| r.get::<_, Option<String>>("stripe_customer_id")))
    }

    /// Reverse-resolve a creator from their platform Customer id
    /// (`cus_…`). Used by `invoice.payment_failed` ingest when the event
    /// carries no `metadata.creator_id` but does carry the `customer` (PR6
    /// Stream-1 infra invoices). Returns `None` if no creator owns that customer.
    pub async fn get_creator_by_customer(
        &self,
        stripe_customer_id: &str,
    ) -> Result<Option<Uuid>, StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT creator_id FROM zeroship.creator_billing WHERE stripe_customer_id = $1",
                &[&stripe_customer_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(rows.first().map(|r| r.get::<_, Uuid>("creator_id")))
    }

    /// Upsert the creator's platform Customer id. Idempotent: re-setting the
    /// same id is a no-op write. The row is created if absent.
    pub async fn set_customer(
        &self,
        creator_id: Uuid,
        stripe_customer_id: &str,
    ) -> Result<(), StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        conn.execute(
            "INSERT INTO zeroship.creator_billing (creator_id, stripe_customer_id) \
             VALUES ($1, $2) \
             ON CONFLICT (creator_id) DO UPDATE \
                SET stripe_customer_id = EXCLUDED.stripe_customer_id, updated_at = NOW()",
            &[&creator_id, &stripe_customer_id],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(())
    }

    /// Mark that the creator has a saved default PaymentMethod (set on the
    /// `setup_intent.succeeded` webhook). Creates the row if absent so a webhook
    /// arriving before any local row still records the fact.
    pub async fn set_default_pm(&self, creator_id: Uuid) -> Result<(), StripeError> {
        let conn = self.registry.conn().await.map_err(|e| StripeError::Db(format!("{e}")))?;
        conn.execute(
            "INSERT INTO zeroship.creator_billing (creator_id, default_pm_set) \
             VALUES ($1, true) \
             ON CONFLICT (creator_id) DO UPDATE SET default_pm_set = true, updated_at = NOW()",
            &[&creator_id],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(())
    }
}

/// Body of `link_account` once we've decided this is a real link
/// (not an idempotent same-account replay). Runs under BEGIN/COMMIT.
async fn link_account_txn(
    conn: &compio_postgres::Client,
    creator_id: Uuid,
    stripe_account_id: &str,
) -> Result<(), StripeError> {
    // Same-account idempotency: a creator double-clicking "Connect
    // Stripe" should NOT pollute the audit history with duplicate rows.
    // FOR UPDATE serializes genuine relinks for creators with a live row.
    let current = conn
        .query(
            "SELECT stripe_account_id FROM zeroship.creator_accounts
             WHERE creator_id = $1 AND unlinked_at IS NULL
             FOR UPDATE",
            &[&creator_id],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
    if let Some(row) = current.first() {
        let current_acct: String = row.get("stripe_account_id");
        if current_acct == stripe_account_id {
            conn.execute(
                "UPDATE zeroship.creator_accounts SET onboarded_at = NOW() WHERE creator_id = $1",
                &[&creator_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
            return Ok(());
        }
    }

    // Close any open history row.
    conn.execute(
        "UPDATE zeroship.creator_account_history SET unlinked_at = NOW()
         WHERE creator_id = $1 AND unlinked_at IS NULL",
        &[&creator_id],
    )
    .await
    .map_err(|e| StripeError::Db(e.to_string()))?;
    // Open new history row.
    conn.execute(
        "INSERT INTO zeroship.creator_account_history(creator_id, stripe_account_id) VALUES($1, $2)",
        &[&creator_id, &stripe_account_id],
    )
    .await
    .map_err(|e| StripeError::Db(e.to_string()))?;
    // Upsert live row.
    conn.execute(
        "INSERT INTO zeroship.creator_accounts(creator_id, stripe_account_id, unlinked_at)
         VALUES($1, $2, NULL)
         ON CONFLICT (creator_id) DO UPDATE
            SET stripe_account_id = EXCLUDED.stripe_account_id,
                onboarded_at = NOW(),
                unlinked_at = NULL",
        &[&creator_id, &stripe_account_id],
    )
    .await
    .map_err(|e| StripeError::Db(e.to_string()))?;
    Ok(())
}

/// Body of `unlink_account` under BEGIN/COMMIT.
async fn unlink_account_txn(
    conn: &compio_postgres::Client,
    creator_id: Uuid,
) -> Result<bool, StripeError> {
    let n = conn
        .execute(
            "UPDATE zeroship.creator_accounts SET unlinked_at = NOW()
             WHERE creator_id = $1 AND unlinked_at IS NULL",
            &[&creator_id],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
    if n > 0 {
        conn.execute(
            "UPDATE zeroship.creator_account_history SET unlinked_at = NOW()
             WHERE creator_id = $1 AND unlinked_at IS NULL",
            &[&creator_id],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
    }
    Ok(n > 0)
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
