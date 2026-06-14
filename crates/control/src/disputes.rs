//! Disputes / chargebacks — the forced-cash-reversal side facts (billing-ops gap #26,
//! PR-8; design `0052 disputes` + flow I + the dispute interaction guard).
//!
//! A cardholder disputes a charge; Stripe fires `charge.dispute.created` and DEBITS the
//! platform's balance immediately (the network holds the funds). This is NOT a refund we
//! initiated — it is a FORCED reversal — so it is its OWN append-only fact, FK→the
//! disputed invoice, mirroring the refund discipline. The cash clawback is recorded as a
//! SEPARATE negative `invoice_payments` row (`kind='dispute_debit'`), so
//! `Σ(invoice_payments)` (PR-3's over-refund anchor) tightens AUTOMATICALLY — a creator
//! can't refund cash that was charged back. No cross-table trigger.
//!
//! The whole flow is driven by the webhook (`stripe_handlers`), claim-after-success on
//! `stripe_events_seen` like every other branch. This module owns the DB-shaped pieces:
//!
//!   * [`DisputeStatus`] — the `dispute_status` domain (`open`/`won`/`lost`) + the mapping
//!     from Stripe's many lifecycle statuses onto it ([`DisputeStatus::from_stripe`]).
//!   * [`resolve_invoice_for_dispute`] — map the disputed Stripe payment object
//!     (`charge`/`payment_intent`, and an optional `invoice` hint) back to the internal
//!     `zeroship.invoices.id`, mirroring how `invoice.paid` resolves it.
//!   * [`record_dispute_created`] — in ONE txn, UPSERT the `billing_disputes` row
//!     (`status='open'`) AND append the negative `dispute_debit` `invoice_payments` row.
//!     Idempotent on the Stripe `du_…` (the `provider_dispute_id` UNIQUE + the dispute
//!     payment-row dedup index), so a redelivery never double-debits.
//!   * [`record_dispute_closed`] — progress the dispute to `won`/`lost`; on `won` append a
//!     compensating positive `dispute_reversal` row restoring the budget. On `lost` the
//!     debit stands.
//!
//! The dispute NEVER auto-issues a refund (the funds already moved) and NEVER mutates the
//! finalized invoice — it is a side fact (plus its signed payment row).

use compio_postgres::GenericClient;

use crate::invoice_payments::{append_dispute_row, DisputePaymentKind};
use crate::registry::RegistryError;

/// The platform-side dispute status (`dispute_status` domain). Stripe's many lifecycle
/// statuses collapse onto these three buckets, which is all the cash model cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisputeStatus {
    /// The dispute is live; the cash is network-held (a `dispute_debit` stands).
    Open,
    /// Funds returned to the platform (a `dispute_reversal` restores the budget).
    Won,
    /// Chargeback final, funds gone (the debit stays).
    Lost,
}

impl DisputeStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Won => "won",
            Self::Lost => "lost",
        }
    }

    /// Map a raw Stripe `dispute.status` onto the platform bucket (verified against
    /// docs.stripe.com/api/disputes/object — status values
    /// `warning_needs_response`/`warning_under_review`/`warning_closed`/`needs_response`/
    /// `under_review`/`won`/`lost`/`prevented`/`charge_refunded`).
    ///
    ///   * `won` (and the inquiry-resolved `warning_closed`/`prevented`) → [`Self::Won`]:
    ///     the funds are with the platform.
    ///   * `lost` → [`Self::Lost`]: the chargeback is final.
    ///   * everything else (`*needs_response`, `*under_review`, `charge_refunded`) → still
    ///     [`Self::Open`]: the cash is network-held pending resolution.
    ///
    /// `charge_refunded` (a legacy status meaning the merchant refunded to make the
    /// dispute moot) stays `open` for our purposes: the cash is gone and the refund path,
    /// if any, is a separate `refunds` fact — we do NOT restore the dispute budget for it.
    #[must_use]
    pub fn from_stripe(status: &str) -> Self {
        match status {
            "won" | "warning_closed" | "prevented" => Self::Won,
            "lost" => Self::Lost,
            _ => Self::Open,
        }
    }

    /// Is this a TERMINAL status (won/lost)? A terminal status drives the `.closed`
    /// handling (resolved_at + the won reversal / lost no-op).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Won | Self::Lost)
    }
}

/// Resolve the internal `zeroship.invoices.id` for a disputed Stripe payment object.
///
/// The dispute object carries `charge` (`ch_…`) and `payment_intent` (`pi_…`) — and, on
/// some API versions, an `invoice` hint — but NOT the `in_…` directly. We resolve by
/// matching ANY of those candidate ids against the provider refs we DID record at payment
/// time, mirroring how `invoice.paid` resolves the internal id:
///
///   1. the `charge` `invoice_payments.provider_ref` (the Stripe payment object recorded
///      when the charge was collected — the very object the dispute is against), then
///   2. `billing_provider_refs(ref_kind='invoice')` (the `in_…` linkage), in case the
///      dispute surfaced the invoice id.
///
/// Returns `None` if no internal invoice maps to any candidate (a Connect end-user charge
/// the platform never invoiced, or a pre-finalize race) — the webhook then acks the event
/// without recording a dispute (nothing to anchor it to).
pub async fn resolve_invoice_for_dispute<C: GenericClient + Sync>(
    conn: &C,
    candidate_refs: &[&str],
) -> Result<Option<String>, RegistryError> {
    for cand in candidate_refs.iter().filter(|c| !c.is_empty()) {
        // (1) Match against a recorded charge payment row's provider_ref.
        let rows = conn
            .query(
                "SELECT invoice_id FROM zeroship.invoice_payments \
                 WHERE kind = 'charge' AND provider_ref = $1 LIMIT 1",
                &[cand],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        if let Some(r) = rows.first() {
            return Ok(Some(r.get::<_, String>("invoice_id")));
        }
        // (2) Match against the invoice-level provider ref (in_…).
        let rows = conn
            .query(
                "SELECT invoice_id FROM zeroship.billing_provider_refs \
                 WHERE provider = 'stripe' AND ref_kind = 'invoice' AND external_id = $1 LIMIT 1",
                &[cand],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        if let Some(r) = rows.first() {
            return Ok(Some(r.get::<_, String>("invoice_id")));
        }
    }
    Ok(None)
}

/// What [`record_dispute_created`] / [`record_dispute_closed`] did, so the webhook can
/// drive the `disputed` notification + audit off it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisputeRecord {
    /// Our `dsp_…` typed id for the dispute row (fresh on first create; the existing id on
    /// a redelivery).
    pub dispute_id: String,
    /// The internal invoice the dispute is against.
    pub invoice_id: String,
    /// The platform status after this event.
    pub status: DisputeStatus,
    /// `true` iff THIS call freshly created the dispute row (drives the once-only
    /// `disputed` notification: a redelivery returns `false`). The notification is
    /// ultimately deduped by the `billing_notifications` claim ledger too — this flag is
    /// the cheap fast-path signal.
    pub newly_created: bool,
}

/// Record a `charge.dispute.created`: in ONE txn, UPSERT the `billing_disputes` row
/// (`status='open'`) and append the negative `dispute_debit` `invoice_payments` row.
///
/// Idempotent on the Stripe `du_…`:
///   * the `billing_disputes.provider_dispute_id` UNIQUE makes the row INSERT a no-op on
///     redelivery (`ON CONFLICT DO NOTHING`),
///   * the dispute payment-row dedup index (0053) makes the `dispute_debit` append a
///     no-op on redelivery.
/// So a redelivered created event NEVER double-debits, even under a fresh `evt_id` that
/// the `stripe_events_seen` gate would not catch.
///
/// `conn` MUST be a live OWNED connection (`&mut`): the upsert + append run in one
/// transaction so the dispute fact and its cash clawback land atomically.
#[allow(clippy::too_many_arguments)]
pub async fn record_dispute_created<C: GenericClient + Sync>(
    conn: &mut C,
    invoice_id: &str,
    amount_cents: i64,
    currency: &str,
    reason: Option<&str>,
    evidence_due_at: Option<chrono::DateTime<chrono::Utc>>,
    provider_dispute_id: &str,
) -> Result<DisputeRecord, RegistryError> {
    if amount_cents <= 0 {
        return Err(RegistryError::InvalidInput(format!(
            "dispute amount must be > 0 (got {amount_cents})"
        )));
    }
    if provider_dispute_id.is_empty() {
        return Err(RegistryError::InvalidInput(
            "dispute requires a non-empty provider_dispute_id (du_…)".to_string(),
        ));
    }

    let tx = conn
        .transaction()
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    // UPSERT the dispute row keyed on the Stripe du_…. ON CONFLICT DO NOTHING: a
    // redelivery returns 0 rows and we read back the existing id.
    let dsp_id = zeroship_core::typed_id::new_dispute_id();
    let inserted = tx
        .query(
            "INSERT INTO zeroship.billing_disputes \
               (id, invoice_id, amount_cents, currency, status, reason, evidence_due_at, \
                provider_dispute_id) \
             VALUES ($1, $2, $3, $4, 'open', $5, $6, $7) \
             ON CONFLICT (provider_dispute_id) DO NOTHING \
             RETURNING id",
            &[
                &dsp_id,
                &invoice_id,
                &amount_cents,
                &currency,
                &reason,
                &evidence_due_at,
                &provider_dispute_id,
            ],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let (dispute_id, newly_created) = if let Some(r) = inserted.first() {
        (r.get::<_, String>("id"), true)
    } else {
        // Redelivery: the row already exists. Read its id (and keep its existing invoice
        // binding — the controlled-update trigger would reject a retarget anyway).
        let existing = tx
            .query(
                "SELECT id FROM zeroship.billing_disputes WHERE provider_dispute_id = $1",
                &[&provider_dispute_id],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        let id = existing
            .first()
            .map(|r| r.get::<_, String>("id"))
            .ok_or_else(|| {
                RegistryError::Database(
                    "dispute ON CONFLICT but no existing row found".to_string(),
                )
            })?;
        (id, false)
    };

    // Append the negative dispute_debit clawback (idempotent on the du_… via the dedup
    // index). −amount lowers Σ(invoice_payments), auto-tightening the over-refund cap.
    append_dispute_row(
        &tx,
        invoice_id,
        -amount_cents,
        currency,
        DisputePaymentKind::Debit,
        provider_dispute_id,
    )
    .await?;

    tx.commit()
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    Ok(DisputeRecord {
        dispute_id,
        invoice_id: invoice_id.to_string(),
        status: DisputeStatus::Open,
        newly_created,
    })
}

/// Record a `charge.dispute.closed` (or any terminal `.updated`): progress the dispute to
/// `won`/`lost` and, on `won`, append the compensating positive `dispute_reversal` row
/// restoring the budget. On `lost` the `dispute_debit` stands (no reversal).
///
/// Idempotent: the `dispute_reversal` append is deduped on the `du_…`, so a redelivered
/// `won` close never double-restores. The status UPDATE is naturally idempotent.
///
/// Returns `None` if no `billing_disputes` row exists for the `du_…` yet (a `.closed`
/// arriving before its `.created`, or for a charge we never recorded) — the webhook acks
/// it (nothing to resolve).
pub async fn record_dispute_closed<C: GenericClient + Sync>(
    conn: &mut C,
    provider_dispute_id: &str,
    status: DisputeStatus,
) -> Result<Option<DisputeRecord>, RegistryError> {
    debug_assert!(status.is_terminal(), "record_dispute_closed expects a terminal status");

    let tx = conn
        .transaction()
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    // Progress the row to the terminal status (controlled-update trigger permits the
    // status/resolved_at change; the frozen columns are held). Return its identity +
    // amount/currency/invoice so we can drive the reversal.
    let updated = tx
        .query(
            "UPDATE zeroship.billing_disputes \
                SET status = $2::text::zeroship.dispute_status, resolved_at = NOW() \
              WHERE provider_dispute_id = $1 \
              RETURNING id, invoice_id, amount_cents, currency",
            &[&provider_dispute_id, &status.as_str()],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let Some(r) = updated.first() else {
        tx.commit()
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        return Ok(None);
    };
    let dispute_id: String = r.get("id");
    let invoice_id: String = r.get("invoice_id");
    let amount_cents: i64 = r.get("amount_cents");
    let currency: String = r.get("currency");

    if status == DisputeStatus::Won {
        // Append the compensating positive reversal (idempotent on the du_…). +amount
        // raises Σ(invoice_payments) back, restoring the refundable-cash budget.
        append_dispute_row(
            &tx,
            &invoice_id,
            amount_cents,
            &currency,
            DisputePaymentKind::Reversal,
            provider_dispute_id,
        )
        .await?;
    }

    tx.commit()
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    Ok(Some(DisputeRecord {
        dispute_id,
        invoice_id,
        status,
        newly_created: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_str_roundtrips() {
        for s in [DisputeStatus::Open, DisputeStatus::Won, DisputeStatus::Lost] {
            // round-trip via the Stripe mapping for the canonical strings
            assert_eq!(DisputeStatus::from_stripe(s.as_str()), s);
        }
    }

    #[test]
    fn stripe_status_mapping() {
        // Open buckets: anything pending response/review.
        for open in ["needs_response", "under_review", "warning_needs_response", "warning_under_review", "charge_refunded", "anything_unknown"] {
            assert_eq!(DisputeStatus::from_stripe(open), DisputeStatus::Open, "{open}");
        }
        // Won buckets: won + inquiry-closed-in-favour.
        for won in ["won", "warning_closed", "prevented"] {
            assert_eq!(DisputeStatus::from_stripe(won), DisputeStatus::Won, "{won}");
        }
        assert_eq!(DisputeStatus::from_stripe("lost"), DisputeStatus::Lost);
    }

    #[test]
    fn terminal_only_for_won_lost() {
        assert!(!DisputeStatus::Open.is_terminal());
        assert!(DisputeStatus::Won.is_terminal());
        assert!(DisputeStatus::Lost.is_terminal());
    }
}
