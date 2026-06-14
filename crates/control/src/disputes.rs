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
//!     (`payment_intent`/`charge`; a Dispute object has NO `invoice` field) back to the
//!     internal `zeroship.invoices.id` via the `pi_…`/`ch_…` `billing_provider_refs`
//!     linkage that the `invoice.paid` handler records at payment time (CRITICAL-1).
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

    /// `true` iff `status` is a Stripe dispute lifecycle value we KNOW (one of the documented
    /// enum members). A value NOT in this set is mapped to [`Self::Open`] by `from_stripe`
    /// but is logged at the call site (MINOR-6) — a new/unknown Stripe status should be
    /// noticed rather than silently bucketed. Kept in lock-step with the docs:
    /// docs.stripe.com/api/disputes/object (`status`).
    #[must_use]
    pub fn is_known_stripe_status(status: &str) -> bool {
        matches!(
            status,
            "warning_needs_response"
                | "warning_under_review"
                | "warning_closed"
                | "needs_response"
                | "under_review"
                | "won"
                | "lost"
                | "prevented"
        )
    }

    /// Is this a TERMINAL status (won/lost)? A terminal status drives the `.closed`
    /// handling (resolved_at + the won reversal / lost no-op).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Won | Self::Lost)
    }
}

/// Resolve the internal `zeroship.invoices.id` a `charge.dispute.*` is against.
///
/// A Stripe Dispute object carries NO `invoice` field (verified against
/// docs.stripe.com/api/disputes/object) — only `charge` (`ch_…`) and `payment_intent`
/// (`pi_…`). Those settle ids are recorded against OUR invoice at `invoice.paid` time as
/// `billing_provider_refs(ref_kind IN ('payment_intent','charge'))` (CRITICAL-1). We
/// resolve by matching the dispute's candidate ids against that linkage in a SINGLE
/// round-trip (MINOR-5):
///
///   * `billing_provider_refs WHERE ref_kind IN ('payment_intent','charge') AND
///     external_id = ANY($candidates)` → the invoice. A `pi_`/`ch_` is GLOBALLY unique at
///     Stripe, and `billing_provider_refs` has `UNIQUE(provider, ref_kind, external_id)`,
///     so this is deterministic (no LIMIT-1 ambiguity). `payment_intent` is preferred over
///     `charge` when both happen to map (ordered in the query) — they point at the same
///     invoice anyway.
///
/// Returns `None` if no internal invoice maps to any candidate (a Connect end-user charge
/// the platform never invoiced, or a pre-`invoice.paid` race) — the webhook then acks the
/// event WITHOUT recording a dispute (nothing to anchor it to), no crash.
pub async fn resolve_invoice_for_dispute<C: GenericClient + Sync>(
    conn: &C,
    candidate_refs: &[&str],
) -> Result<Option<String>, RegistryError> {
    let candidates: Vec<&str> = candidate_refs
        .iter()
        .copied()
        .filter(|c| !c.is_empty())
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    // ONE round-trip: match any candidate against the pi_/ch_ linkage recorded at
    // invoice.paid. ORDER BY puts 'payment_intent' before 'charge' so a deterministic
    // winner is returned if (pathologically) both kinds resolve.
    let rows = conn
        .query(
            "SELECT invoice_id FROM zeroship.billing_provider_refs \
             WHERE provider = 'stripe' \
               AND ref_kind IN ('payment_intent','charge') \
               AND external_id = ANY($1) \
             ORDER BY (ref_kind = 'payment_intent') DESC \
             LIMIT 1",
            &[&candidates],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map(|r| r.get::<_, String>("invoice_id")))
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
/// ORDER-INDEPENDENT (MAJOR-4): if a TERMINAL row already exists because `.closed` arrived
/// FIRST (close-before-create), the `ON CONFLICT DO NOTHING` no-ops the insert, the debit
/// append is idempotent (already applied), and we reconcile to the existing row WITHOUT
/// touching its terminal status. The `.created` therefore never resurrects a resolved
/// dispute back to `open`; the end state is identical to in-order delivery.
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

/// Context the close handler resolves from the `charge.dispute.closed`/`.updated` event so
/// it can CREATE the dispute terminal if the `.created` never arrived (MAJOR-4). All of it
/// is carried on the dispute object Stripe delivers on the close event.
#[derive(Debug, Clone)]
pub struct DisputeCloseContext<'a> {
    /// The internal invoice the dispute is against, resolved via the pi_/ch_ linkage.
    pub invoice_id: &'a str,
    /// The disputed (clawed-back) amount in cents — used only to seed a close-before-create
    /// terminal row (and its debit). Must be `> 0` when creating.
    pub amount_cents: i64,
    pub currency: &'a str,
    pub reason: Option<&'a str>,
}

/// Record a `charge.dispute.closed` (or terminal `.updated`): progress the dispute to its
/// terminal status and reconcile its cash. ORDER-INDEPENDENT and LIFECYCLE-SAFE:
///
///   * Normal order (`.created` already landed `open`): the row is UPDATEd
///     `open → won|lost` (gated `WHERE status='open'`, so a redelivered/late terminal event
///     is a no-op). On `won` the compensating positive `dispute_reversal` is appended
///     (idempotent on the `du_…`); on `lost` the debit stands.
///
///   * CLOSE-BEFORE-CREATE (MAJOR-4 — legal at-least-once reordering): no row exists yet,
///     so we UPSERT a FRESH row DIRECTLY in the terminal status, applying the `dispute_debit`
///     (always) AND, on `won`, the `dispute_reversal` (so net cash is restored). The later
///     `.created` then finds a terminal row and reconciles to a no-op
///     ([`record_dispute_created`]). End state is identical regardless of delivery order:
///     debit applied; reversal iff won.
///
///   * Already terminal (redelivery, or the `won→lost` reorder the trigger forbids): the
///     gated UPDATE matches 0 rows and the row already exists — we return it UNCHANGED
///     (no status flip, no extra cash movement). This is the CRITICAL-3 guard: a stale
///     `lost` after a `won` can NEVER strand restored cash on a lost dispute.
///
/// `ctx` is `None` only when the caller could not resolve the invoice (an unrecorded
/// charge); then a close-before-create returns `Ok(None)` (nothing to anchor) rather than
/// inventing a row.
pub async fn record_dispute_closed<C: GenericClient + Sync>(
    conn: &mut C,
    provider_dispute_id: &str,
    status: DisputeStatus,
    ctx: Option<DisputeCloseContext<'_>>,
) -> Result<Option<DisputeRecord>, RegistryError> {
    debug_assert!(status.is_terminal(), "record_dispute_closed expects a terminal status");

    let tx = conn
        .transaction()
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    // (1) Progress an OPEN row to the terminal status. The `WHERE status='open'` gate makes
    // a redelivered/late terminal event (the row is already terminal) a 0-row no-op — and
    // the controlled-update trigger independently rejects any terminal→* flip, so a stale
    // `lost` after a `won` can never strand restored cash (CRITICAL-3).
    let updated = tx
        .query(
            "UPDATE zeroship.billing_disputes \
                SET status = $2::text::zeroship.dispute_status, resolved_at = NOW() \
              WHERE provider_dispute_id = $1 AND status = 'open' \
              RETURNING id, invoice_id, amount_cents, currency",
            &[&provider_dispute_id, &status.as_str()],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    if let Some(r) = updated.first() {
        let dispute_id: String = r.get("id");
        let invoice_id: String = r.get("invoice_id");
        let amount_cents: i64 = r.get("amount_cents");
        let currency: String = r.get("currency");
        if status == DisputeStatus::Won {
            // +amount raises Σ(invoice_payments) back, restoring the refundable-cash budget.
            append_dispute_row(
                &tx, &invoice_id, amount_cents, &currency, DisputePaymentKind::Reversal,
                provider_dispute_id,
            )
            .await?;
        }
        tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        return Ok(Some(DisputeRecord { dispute_id, invoice_id, status, newly_created: false }));
    }

    // (2) No OPEN row moved. Either the row is ALREADY terminal (redelivery / a forbidden
    // reorder) — return it unchanged — or NO row exists at all (close-before-create).
    let existing = tx
        .query(
            "SELECT id, invoice_id, amount_cents, currency, status::text AS status \
               FROM zeroship.billing_disputes WHERE provider_dispute_id = $1",
            &[&provider_dispute_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    if let Some(r) = existing.first() {
        // Already terminal: a no-op. Surface the EXISTING terminal status (NOT the incoming
        // one) so a stale `lost` after a `won` reports `won` — the cash state is coherent.
        let dispute_id: String = r.get("id");
        let invoice_id: String = r.get("invoice_id");
        let existing_status = DisputeStatus::from_stripe(&r.get::<_, String>("status"));
        tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        return Ok(Some(DisputeRecord {
            dispute_id,
            invoice_id,
            status: existing_status,
            newly_created: false,
        }));
    }

    // (3) CLOSE-BEFORE-CREATE: no row at all. UPSERT a fresh row DIRECTLY terminal, applying
    // the debit (always) + the won reversal — but only if we resolved the anchor invoice.
    let Some(ctx) = ctx else {
        tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        return Ok(None);
    };
    if ctx.amount_cents <= 0 {
        return Err(RegistryError::InvalidInput(format!(
            "dispute amount must be > 0 to create a terminal dispute (got {})",
            ctx.amount_cents
        )));
    }
    let dsp_id = zeroship_core::typed_id::new_dispute_id();
    let inserted = tx
        .query(
            "INSERT INTO zeroship.billing_disputes \
               (id, invoice_id, amount_cents, currency, status, reason, provider_dispute_id, \
                resolved_at) \
             VALUES ($1, $2, $3, $4, $5::text::zeroship.dispute_status, $6, $7, NOW()) \
             ON CONFLICT (provider_dispute_id) DO NOTHING \
             RETURNING id",
            &[
                &dsp_id,
                &ctx.invoice_id,
                &ctx.amount_cents,
                &ctx.currency,
                &status.as_str(),
                &ctx.reason,
                &provider_dispute_id,
            ],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    // A concurrent create could have raced in between our probe and this INSERT; if so the
    // ON CONFLICT no-ops and we read the existing id (its status is whatever that create
    // set — left to the normal create/close reconciliation, no flip here).
    let (dispute_id, created_terminal) = if let Some(r) = inserted.first() {
        (r.get::<_, String>("id"), true)
    } else {
        let row = tx
            .query(
                "SELECT id FROM zeroship.billing_disputes WHERE provider_dispute_id = $1",
                &[&provider_dispute_id],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        let id = row.first().map(|r| r.get::<_, String>("id")).ok_or_else(|| {
            RegistryError::Database("dispute ON CONFLICT but no existing row found".to_string())
        })?;
        (id, false)
    };

    // Apply the cash facts. Both appends are idempotent on the du_…, so even if a racing
    // create already appended the debit this is a no-op.
    append_dispute_row(
        &tx, ctx.invoice_id, -ctx.amount_cents, ctx.currency, DisputePaymentKind::Debit,
        provider_dispute_id,
    )
    .await?;
    if status == DisputeStatus::Won {
        append_dispute_row(
            &tx, ctx.invoice_id, ctx.amount_cents, ctx.currency, DisputePaymentKind::Reversal,
            provider_dispute_id,
        )
        .await?;
    }
    tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(Some(DisputeRecord {
        dispute_id,
        invoice_id: ctx.invoice_id.to_string(),
        status,
        newly_created: created_terminal,
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
