//! Server-held, per-creator application-fee policy (billing G1, Stream-2).
//!
//! Stream-2 is the CREATOR-REVENUE rail: a creator charges THEIR end-users via
//! their own Stripe **Connect** account; the platform takes a fee, stamped on
//! the Connect charge SERVER-SIDE. Previously the fee was chosen CLIENT-SIDE by
//! the SDK (`applicationFeePercent ?? 15`) — any creator could set it to 0
//! (ISS-29). This module makes the fee SERVER-AUTHORITATIVE:
//!
//! * [`FeePolicy`] is a pure value with [`FeePolicy::fee_cents`] — integer cents,
//!   round-once, with optional cap/floor clamping. Unit-tested exhaustively.
//! * [`FeePolicyStore`] persists it in `zeroship.creator_fee_policy` (changeset
//!   0044). `get` returns the in-code DEFAULT (`Percent{1500}` = 15%) when no row
//!   exists, so a brand-new creator still pays 15% without a signup-time INSERT.
//! * `set` is the OPERATOR-ONLY write — the handler gates it on Cedar
//!   `BillingWrite` over `Resource::Any` (a creator self-editing their own fee is
//!   a privilege escalation). There is NO creator-reachable path to lower it.

use uuid::Uuid;

use crate::registry::Registry;
use crate::stripe_store::StripeError;

/// Basis points denominator: 10_000 bps = 100%.
const BPS_DENOM: u128 = 10_000;

/// The platform's default fee when a creator has no explicit policy row: 15%.
pub const DEFAULT_PERCENT_BPS: i32 = 1500;

/// A per-creator application-fee policy. Server-held; never client-set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeePolicy {
    /// A flat fee per charge, in cents.
    Fixed { amount_cents: u64 },
    /// `bps` basis points of the transaction (1500 = 15%), clamped to
    /// `[floor, cap]` when those bounds are present.
    Percent {
        bps: u32,
        cap_cents: Option<u64>,
        floor_cents: Option<u64>,
    },
}

impl FeePolicy {
    /// The platform default: 15%, no cap/floor.
    #[must_use]
    pub const fn default_percent() -> Self {
        Self::Percent {
            bps: DEFAULT_PERCENT_BPS as u32,
            cap_cents: None,
            floor_cents: None,
        }
    }

    /// Compute the application fee (in cents) for a transaction of `txn_cents`.
    ///
    /// * `Fixed`   → the flat amount, independent of `txn_cents` (but never more
    ///   than the txn itself — a fee may not exceed the charge).
    /// * `Percent` → `round(txn × bps / 10000)` (round-half-up, computed ONCE in
    ///   integer `u128` to avoid float drift), clamped to `[floor, cap]`. The
    ///   final fee is additionally capped at `txn_cents` (the fee can never
    ///   exceed the charge — Stripe rejects `application_fee_amount > amount`).
    #[must_use]
    pub fn fee_cents(&self, txn_cents: u64) -> u64 {
        let raw = match *self {
            Self::Fixed { amount_cents } => amount_cents,
            Self::Percent {
                bps,
                cap_cents,
                floor_cents,
            } => {
                // round-half-up in integer math: (txn*bps + 5000) / 10000.
                let product = u128::from(txn_cents) * u128::from(bps);
                let rounded = (product + (BPS_DENOM / 2)) / BPS_DENOM;
                // Clamp to [floor, cap] (floor applied first, then cap; if a
                // misconfigured policy has floor > cap the cap wins — the fee
                // never exceeds the cap).
                let mut fee = rounded;
                if let Some(floor) = floor_cents {
                    fee = fee.max(u128::from(floor));
                }
                if let Some(cap) = cap_cents {
                    fee = fee.min(u128::from(cap));
                }
                // u128 → u64 saturating (a cap/percent that overflows u64 is not
                // physically possible for real money, but never wrap).
                u64::try_from(fee).unwrap_or(u64::MAX)
            }
        };
        // A fee may never exceed the charge.
        raw.min(txn_cents)
    }
}

/// PG-backed store for [`FeePolicy`]. Operator writes only (gated at the handler).
#[allow(missing_debug_implementations)]
pub struct FeePolicyStore {
    registry: Registry,
}

impl FeePolicyStore {
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// The fee policy for `creator_id`. Returns the in-code DEFAULT
    /// (`Percent{1500}` = 15%, no cap/floor) when no row exists — a creator with
    /// no explicit policy still pays 15% without a signup-time INSERT.
    ///
    /// A row with an unrecognised/inconsistent shape (which the CHECK constraint
    /// already prevents on write) is a corrupt state; we fail CLOSED to the 15%
    /// default rather than silently zero the fee.
    pub async fn get(&self, creator_id: Uuid) -> Result<FeePolicy, StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT kind, amount_cents, percent_bps, cap_cents, floor_cents \
                 FROM zeroship.creator_fee_policy WHERE creator_id = $1",
                &[&creator_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        let Some(row) = rows.first() else {
            return Ok(FeePolicy::default_percent());
        };
        let kind: String = row.get("kind");
        match kind.as_str() {
            "fixed" => {
                let amount: Option<i64> = row.get("amount_cents");
                match amount {
                    Some(a) if a >= 0 => Ok(FeePolicy::Fixed {
                        amount_cents: a as u64,
                    }),
                    // CHECK prevents this; fail closed to default if it ever happens.
                    _ => Ok(FeePolicy::default_percent()),
                }
            }
            "percent" => {
                let bps: Option<i32> = row.get("percent_bps");
                let cap: Option<i64> = row.get("cap_cents");
                let floor: Option<i64> = row.get("floor_cents");
                match bps {
                    Some(b) if (0..=10_000).contains(&b) => Ok(FeePolicy::Percent {
                        bps: b as u32,
                        cap_cents: cap.and_then(|c| u64::try_from(c).ok()),
                        floor_cents: floor.and_then(|f| u64::try_from(f).ok()),
                    }),
                    _ => Ok(FeePolicy::default_percent()),
                }
            }
            _ => Ok(FeePolicy::default_percent()),
        }
    }

    /// Upsert the fee policy for `creator_id`. OPERATOR-ONLY: the caller MUST
    /// have gated on Cedar `BillingWrite`/`Resource::Any` before invoking this —
    /// a creator may never set/lower their own fee. Idempotent (ON CONFLICT
    /// UPDATE).
    pub async fn set(&self, creator_id: Uuid, policy: FeePolicy) -> Result<(), StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        let (kind, amount, bps, cap, floor): (
            &str,
            Option<i64>,
            Option<i32>,
            Option<i64>,
            Option<i64>,
        ) = match policy {
            FeePolicy::Fixed { amount_cents } => {
                let a = i64::try_from(amount_cents).map_err(|_| {
                    StripeError::Validation("fixed fee amount_cents exceeds i64::MAX".into())
                })?;
                ("fixed", Some(a), None, None, None)
            }
            FeePolicy::Percent {
                bps,
                cap_cents,
                floor_cents,
            } => {
                if bps > 10_000 {
                    return Err(StripeError::Validation(
                        "percent fee bps must be in [0, 10000]".into(),
                    ));
                }
                let cap = cap_cents
                    .map(|c| i64::try_from(c))
                    .transpose()
                    .map_err(|_| StripeError::Validation("cap_cents exceeds i64::MAX".into()))?;
                let floor = floor_cents
                    .map(|f| i64::try_from(f))
                    .transpose()
                    .map_err(|_| StripeError::Validation("floor_cents exceeds i64::MAX".into()))?;
                ("percent", None, Some(bps as i32), cap, floor)
            }
        };
        conn.execute(
            "INSERT INTO zeroship.creator_fee_policy \
                (creator_id, kind, amount_cents, percent_bps, cap_cents, floor_cents, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, NOW()) \
             ON CONFLICT (creator_id) DO UPDATE SET \
                kind = EXCLUDED.kind, \
                amount_cents = EXCLUDED.amount_cents, \
                percent_bps = EXCLUDED.percent_bps, \
                cap_cents = EXCLUDED.cap_cents, \
                floor_cents = EXCLUDED.floor_cents, \
                updated_at = NOW()",
            &[&creator_id, &kind, &amount, &bps, &cap, &floor],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_fifteen_percent() {
        let p = FeePolicy::default_percent();
        assert_eq!(p, FeePolicy::Percent { bps: 1500, cap_cents: None, floor_cents: None });
        // 15% of $100.00 = $15.00.
        assert_eq!(p.fee_cents(10_000), 1_500);
    }

    #[test]
    fn fixed_fee_ignores_txn_size() {
        let p = FeePolicy::Fixed { amount_cents: 50 };
        assert_eq!(p.fee_cents(10_000), 50);
        assert_eq!(p.fee_cents(1_000_000), 50);
        // …but never exceeds the charge itself.
        assert_eq!(p.fee_cents(30), 30);
    }

    #[test]
    fn percent_basic() {
        let p = FeePolicy::Percent { bps: 1500, cap_cents: None, floor_cents: None };
        assert_eq!(p.fee_cents(10_000), 1_500); // 15%
        assert_eq!(p.fee_cents(0), 0);
        let p10 = FeePolicy::Percent { bps: 1000, cap_cents: None, floor_cents: None };
        assert_eq!(p10.fee_cents(2_550), 255); // exactly 10%
    }

    #[test]
    fn percent_rounds_half_up_once() {
        // 15% of 99 = 14.85 → round-half-up → 15.
        let p = FeePolicy::Percent { bps: 1500, cap_cents: None, floor_cents: None };
        assert_eq!(p.fee_cents(99), 15);
        // 15% of 103 = 15.45 → 15.
        assert_eq!(p.fee_cents(103), 15);
        // exact half: 10% of 5 = 0.5 → round-half-up → 1.
        let p10 = FeePolicy::Percent { bps: 1000, cap_cents: None, floor_cents: None };
        assert_eq!(p10.fee_cents(5), 1);
    }

    #[test]
    fn percent_fee_clamped_to_cap() {
        // 15% of $1000 = $150, but cap at $100 (10000 cents).
        let p = FeePolicy::Percent { bps: 1500, cap_cents: Some(10_000), floor_cents: None };
        assert_eq!(p.fee_cents(100_000), 10_000);
        // Below the cap: unclamped.
        assert_eq!(p.fee_cents(10_000), 1_500);
    }

    #[test]
    fn percent_fee_clamped_to_floor() {
        // 15% of $1.00 = 15 cents, but floor at 50 cents.
        let p = FeePolicy::Percent { bps: 1500, cap_cents: None, floor_cents: Some(50) };
        assert_eq!(p.fee_cents(100), 50);
        // Above the floor: unclamped.
        assert_eq!(p.fee_cents(10_000), 1_500);
    }

    #[test]
    fn percent_floor_capped_by_txn() {
        // Floor 50 but the charge is only 30 → fee can't exceed the charge.
        let p = FeePolicy::Percent { bps: 1500, cap_cents: None, floor_cents: Some(50) };
        assert_eq!(p.fee_cents(30), 30);
    }

    #[test]
    fn bps_zero_is_free() {
        let p = FeePolicy::Percent { bps: 0, cap_cents: None, floor_cents: None };
        assert_eq!(p.fee_cents(10_000), 0);
    }

    #[test]
    fn bps_full_is_whole_charge() {
        let p = FeePolicy::Percent { bps: 10_000, cap_cents: None, floor_cents: None };
        assert_eq!(p.fee_cents(10_000), 10_000);
    }
}
