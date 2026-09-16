//! Void + reissue + the negative-invoice true-up bridge.
//!
//! A void releases the `(organization, period)` claim, while `invoice_payments` stores
//! cash-collected side facts. This module owns the operator void path, the
//! `void_reversal` credit-conservation entry, the reissue, and the true-up.
//!
//! ## The correction model
//!
//! A finalized invoice is NEVER edited in place (the immutability trigger permits
//! exactly one transition: `finalized → void`, with the four money columns held
//! equal). To correct a wrong bill, an operator VOIDS it and the reconciler reissues a
//! fresh, re-priced invoice into the released period slot.
//!
//! ## Re-drivable to completion
//!
//! The sequence is three phases — Phase 1 (void + `void_reversal`), Phase 2 (reissue),
//! Phase 3 (true-up) — and each MONEY mutation is individually serialized per organization:
//!   * Phase 1 takes the per-organization advisory lock and appends `void_reversal` + flips
//!     the invoice to `void` in ONE txn.
//!   * Phase 2's `bill_organization` re-acquires the SAME lock inside its own `consume_at_finalize`
//!     txn (a separate session — it CANNOT share Phase 1's txn, so a single all-phases
//!     transaction would self-deadlock; hence the phases stay separate but each is locked).
//!   * Phase 3's true-up refund takes the SAME lock inside `claim_refund_locked`.
//!
//! Crucially the WHOLE operation is RE-DRIVABLE: [`void_and_reissue`] accepts an
//! ALREADY-VOID invoice and converges the reissue + true-up tail. A crash between the
//! Phase-1 commit and Phase 3 therefore is NOT terminal — re-invoking the endpoint (or a
//! sweep) on the voided invoice completes the reissue + true-up idempotently:
//!   * Phase 1 is skipped when the invoice is already void (the `already_reversed` guard
//!     also makes the `void_reversal` append a no-op on re-drive);
//!   * Phase 2's `bill_organization` short-circuits on an already-finalized active invoice for
//!     the period (it never double-reissues);
//!   * Phase 3's true-up is idempotency-keyed on `trueup:{invoice_id}` (it never
//!     double-refunds).
//!
//! So a re-invocation observes the same `VoidReissueOutcome` and the balance is conserved.
//!
//! ## void_reversal — conserving consumed credit
//!
//! When the voided invoice consumed credit, voiding it WITHOUT restoring that credit
//! would leave the reissue re-consuming from a balance that is short by the voided
//! invoice's draw → silent money loss + a double-consume. So, INSIDE the void txn, we
//! append a compensating positive `void_reversal` entry for every `consumed` entry the
//! voided invoice drew (`-c.amount_cents` flips each negative `consumed` back to a
//! positive restore). `Σ(credit_ledger)` is then exactly what it was before the voided
//! invoice consumed anything; the reissue re-draws from the restored balance and the
//! balance is conserved end-to-end.
//!
//! ## The true-up bridge
//!
//! A reissue can be LOWER than what was already collected on the voided invoice. The
//! over-collection must come back to the organization. The bridge auto-issues a cash refund
//! against the VOIDED invoice of:
//!
//! ```text
//! over = cash_paid(old) − cash_refunds_already_issued(old) − total(new), floored at 0
//! ```
//!
//! The `cash_refunds_already_issued` subtraction is required: a calculation of
//! `cash_paid(old) − total(new)` would IGNORE earlier refunds, so on an invoice
//! already partially cash-refunded the bridge would over-refund — and its own
//! over-refund trigger would then REJECT the bridge refund, leaving the
//! over-collection stuck. Subtracting the already-issued cash refunds makes the cap
//! satisfied by construction:
//! `cash_refunds_already_issued + over = cash_paid(old) − total(new) ≤ cash_paid(old)`.
//!
//! That `over` is recomputed INSIDE `refund::issue_true_up_refund`'s
//! per-organization-locked claim transaction from the live cash anchor — Phase 3 passes
//! only the immutable `reissued_total` and never pre-reads the cash. A concurrent
//! refund/dispute landing between Phase 2 and the claim therefore cannot make the
//! claimed amount stale, trip the over-refund trigger, or under-refund.

use compio_postgres::GenericClient;

use crate::cron::billing_reconcile;
use crate::refund::{self, RefundOutcome};
use crate::registry::RegistryError;
use crate::stripe_client::StripeApi;
use crate::AppState;

/// The outcome of [`void_and_reissue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoidReissueOutcome {
    /// The voided invoice id (the audit record).
    pub voided_invoice_id: String,
    /// The reissued invoice id, if the reconciler produced a new bill for the period
    /// (a corrected period with usage). `None` if the reissue priced to $0 / no
    /// billable usage (e.g. the only app's usage was the mis-attribution being
    /// corrected away) — the period then carries no active invoice.
    pub reissued_invoice_id: Option<String>,
    /// The true-up refund id, if the reissue was lower than the cash collected on the
    /// voided invoice and an over-collection refund was auto-issued.
    pub true_up_refund_id: Option<String>,
    /// The cents auto-refunded by the true-up bridge (0 if none).
    pub true_up_cents: i64,
}

/// The per-organization advisory-lock key, IDENTICAL to the one
/// [`crate::credit::consume_at_finalize`] takes (`pg_advisory_xact_lock(
/// hashtext(organization_id::text)::bigint)`), so void+reissue serializes against a
/// concurrent reconcile consume/finalize for the same organization. Taken as the first act
/// of the void txn.
async fn take_per_organization_lock<C: GenericClient + Sync>(
    conn: &C,
    organization_id: &str,
) -> Result<(), RegistryError> {
    conn.execute(
        "SELECT pg_advisory_xact_lock(hashtext($1::text)::bigint)",
        &[&organization_id.to_string()],
    )
    .await
    .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(())
}

/// Void a finalized invoice and reissue a corrected one for the same `(organization,
/// period)`, conserving consumed credit (`void_reversal`) and auto-refunding any
/// over-collection (the true-up bridge). Operator-only (the caller gates authz).
///
/// Generic over the [`StripeApi`] so the reissue's reconcile + the true-up's cash
/// refund drive a test's recording fake. The cash leg of the true-up refund uses the
/// [`refund::StripeRefundProvider`] over `stripe`.
#[allow(clippy::future_not_send)]
pub async fn void_and_reissue<S: StripeApi>(
    state: &AppState,
    stripe: &S,
    invoice_id: &str,
) -> Result<VoidReissueOutcome, RegistryError> {
    // Read the invoice to void: must exist + be finalized; capture organization/period.
    let mut conn = state.registry.conn().await?;
    let inv = conn
        .query(
            "SELECT organization_id, period, status, \
                    subtotal_cents, credit_cents, tax_cents, total_cents \
             FROM zeroship.invoices WHERE id = $1",
            &[&invoice_id],
        )
        .await?;
    let Some(row) = inv.first() else {
        return Err(RegistryError::InvalidInput(format!("no invoice {invoice_id}")));
    };
    let organization_id: String = row.get("organization_id");
    let period: chrono::NaiveDate = row.get("period");
    let status: String = row.get("status");
    // A `finalized` invoice is voided then reissued. An ALREADY-`void` invoice
    // is a RE-DRIVE of a crash between the Phase-1 commit and Phase 3 — we skip Phase 1
    // (it is already void + reversed) and converge the reissue + true-up tail. A `draft`
    // invoice is never voidable.
    if status != "finalized" && status != "void" {
        return Err(RegistryError::InvalidInput(format!(
            "invoice {invoice_id} is {status}, not finalized — only a finalized invoice can be voided"
        )));
    }
    let needs_void = status == "finalized";

    // ── Phase 1: void + void_reversal, in ONE txn, under the per-organization lock ──
    // Skipped on a re-drive (already void): the `void_reversal` is already appended and
    // the invoice is already flipped — Phase 1 has nothing left to do.
    if needs_void {
        let tx = conn.transaction().await?;
        take_per_organization_lock(&tx, &organization_id).await?;

        // Restore the credit the voided invoice consumed BEFORE it is voided,
        // so the reissue re-consumes from the restored balance. One positive `void_reversal`
        // per `consumed` row the voided invoice drew (`-c.amount_cents` flips negative→positive,
        // matching the kind↔sign + grant-ref CHECKs). Idempotent: if void_reversal entries
        // already exist for this invoice (a re-drive that crashed mid-Phase-1), skip the append.
        let already_reversed = tx
            .query(
                "SELECT 1 FROM zeroship.credit_ledger \
                 WHERE applied_invoice_id = $1 AND kind = 'void_reversal' LIMIT 1",
                &[&invoice_id],
            )
            .await?;
        if already_reversed.is_empty() {
            // Mint the ids in Rust (no in-DB base36 generator) — one per consumed row.
            let consumed = tx
                .query(
                    "SELECT amount_cents, currency, consumed_from_grant_id \
                     FROM zeroship.credit_ledger \
                     WHERE applied_invoice_id = $1 AND kind = 'consumed'",
                    &[&invoice_id],
                )
                .await?;
            for c in &consumed {
                let amount: i64 = c.get("amount_cents"); // negative
                let currency: String = c.get("currency");
                // MINOR: a `consumed` row ALWAYS names its grant (the 0048
                // credit_ledger_grant_ref CHECK requires it; PR-2 always sets it). Guard
                // defensively: a NULL would violate that CHECK on the void_reversal INSERT
                // and abort the whole void cryptically — surface it clearly instead.
                let Some(grant_id) = c.get::<_, Option<String>>("consumed_from_grant_id") else {
                    return Err(RegistryError::Database(format!(
                        "consumed credit_ledger row on invoice {invoice_id} has NULL \
                         consumed_from_grant_id — cannot build its void_reversal (0048 grant-ref \
                         CHECK would abort the void); data is inconsistent"
                    )));
                };
                let entry_id = zeroship_core::typed_id::new_credit_id();
                tx.execute(
                    "INSERT INTO zeroship.credit_ledger \
                       (id, organization_id, kind, amount_cents, currency, applied_invoice_id, \
                        consumed_from_grant_id) \
                     VALUES ($1, $2, 'void_reversal', $3, $4, $5, $6)",
                    &[&entry_id, &organization_id, &(-amount), &currency, &invoice_id, &grant_id],
                )
                .await?;
            }
        }

        // The finalized→void transition: status='void', money columns held EQUAL (the
        // immutability trigger permits exactly this). voided_at=NOW(). The payment rows
        // stay in place as the voided invoice's permanent cash record.
        tx.execute(
            "UPDATE zeroship.invoices \
             SET status = 'void', voided_at = NOW(), updated_at = NOW() WHERE id = $1",
            &[&invoice_id],
        )
        .await?;
        tx.commit().await?;
    }

    // ── Phase 2: reissue via the REAL reconciler path for the same (organization, period) ──
    // The void released the period claim (the partial unique index `WHERE status <>
    // 'void'`), so bill_organization can claim + re-price + re-consume + finalize a fresh
    // invoice. It takes the per-organization advisory lock again (inside consume), so it
    // can't race a concurrent reconcile.
    let period_start = period_start_unix_for(period);
    let pricing = crate::pricing_store::PricingStore::new(state.registry.clone());
    let weights = pricing.weights().await?;
    let default_fx = pricing.default_fx_pico_cents_per_unit().await?;
    let app_ids = billing_reconcile::owned_app_ids(state, &organization_id).await?;

    let _reissued = billing_reconcile::bill_organization(
        state, stripe, &weights, default_fx, &organization_id, &app_ids, period_start,
    )
    .await?;

    // Read back the reissued (active, non-void) invoice id + total for this period.
    let mut conn2 = state.registry.conn().await?;
    let reissued = conn2
        .query(
            "SELECT id, total_cents FROM zeroship.invoices \
             WHERE organization_id = $1 AND period = $2::date AND status <> 'void'",
            &[&organization_id, &period],
        )
        .await?;
    let reissued_invoice_id: Option<String> = reissued.first().map(|r| r.get::<_, String>("id"));
    let reissued_total: i64 = reissued.first().map_or(0, |r| r.get::<_, i64>("total_cents"));

    // ── Phase 3: the true-up bridge ──
    // over = cash_paid(old) − cash_refunds_already_issued(old) − total(new), floored at 0.
    //
    // The over-collection is recomputed INSIDE `issue_true_up_refund`'s per-organization-locked
    // claim txn from the live cash anchor — NOT pre-read here — so a concurrent refund/dispute
    // landing between Phase 2 and Phase 3 cannot make the claimed amount stale. We pass only the
    // immutable `reissued_total`; the bridge returns the actual cents refunded (0 if the
    // over-collection vanished under the lock). The idempotency key (old_invoice_id, 'true_up')
    // makes a re-drive of the same void+reissue not double-refund. The cap holds by construction:
    // cash_refunds_already_issued + over = cash_paid(old) − total(new) ≤ cash_paid(old).
    let idem = format!("trueup:{invoice_id}");
    let provider = refund::StripeRefundProvider { stripe };
    let outcome = refund::issue_true_up_refund(
        &mut conn2,
        &provider,
        invoice_id,
        reissued_total,
        Some("true-up: over-collection on voided invoice"),
        &idem,
    )
    .await?;
    let (true_up_refund_id, true_up_cents) = match outcome {
        // A real over-collection was refunded — read the cents off the claimed refund row so
        // the reported amount reflects what was actually issued under the lock.
        RefundOutcome::Issued { refund_id, .. } => {
            let cents = true_up_amount_for(&conn2, &refund_id).await?;
            (Some(refund_id), cents)
        }
        // Duplicate carries either a real prior refund id (re-drive) or the empty sentinel
        // (no over-collection under the lock → nothing refunded).
        RefundOutcome::Duplicate(refund_id) => {
            if refund_id.is_empty() {
                (None, 0)
            } else {
                let cents = true_up_amount_for(&conn2, &refund_id).await?;
                (Some(refund_id), cents)
            }
        }
        RefundOutcome::OverRefund(msg) => {
            // Should be impossible by construction (the amount is recomputed under the lock);
            // surface loudly rather than silently leaving the organization out-of-pocket.
            return Err(RegistryError::Database(format!(
                "true-up over-refund rejected (BUG — cap math): {msg}"
            )));
        }
        RefundOutcome::Conflict => {
            return Err(RegistryError::Database(
                "true-up idempotency conflict — a prior true-up with a different body exists"
                    .to_string(),
            ));
        }
        RefundOutcome::InvalidInvoice(msg) => {
            return Err(RegistryError::Database(format!("true-up invalid invoice: {msg}")));
        }
    };

    Ok(VoidReissueOutcome {
        voided_invoice_id: invoice_id.to_string(),
        reissued_invoice_id,
        true_up_refund_id,
        true_up_cents,
    })
}

/// Read the cents actually refunded by a true-up `refunds` row — the authoritative amount,
/// since the over-collection is computed UNDER the lock inside `issue_true_up_refund` (H1) and
/// is no longer known to this caller before the claim. Returns 0 if the row is absent.
async fn true_up_amount_for<C: GenericClient + Sync>(
    conn: &C,
    refund_id: &str,
) -> Result<i64, RegistryError> {
    let rows = conn
        .query(
            "SELECT amount_cents FROM zeroship.refunds WHERE id = $1",
            &[&refund_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map_or(0, |r| r.get::<_, i64>("amount_cents")))
}

/// Unix-seconds start of a `billing_period` DATE (the first-of-month, UTC midnight).
/// The inverse of `metering::period_date`, used to drive `bill_organization` (which takes
/// `period_start` unix seconds).
fn period_start_unix_for(period: chrono::NaiveDate) -> i64 {
    use chrono::TimeZone;
    let midnight = period.and_hms_opt(0, 0, 0).expect("midnight is always valid");
    chrono::Utc.from_utc_datetime(&midnight).timestamp()
}
