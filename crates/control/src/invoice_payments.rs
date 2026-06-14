//! Invoice payments — the append-only cash-collected side facts (billing-ops gap
//! #26, PR-1; design round 2 CRITICAL-A).
//!
//! Cash-collected for a finalized invoice is `Σ(invoice_payments.amount_cents)`,
//! NEVER a column on the frozen invoice — writing such a column is mechanically
//! impossible against `invoices_immutable()` (which RAISEs on every
//! finalized→finalized UPDATE). A payment is therefore an APPEND-ONLY SIDE FACT
//! against the invoice: this module owns the two operations PR-1 needs —
//!
//!   * [`cash_collected`] — `Σ(amount_cents)` over an invoice's payment rows. This
//!     is the over-refund anchor consumed by PR-3's `refunds_no_over_refund`
//!     trigger (which inlines the same SELECT) and the true-up bridge.
//!   * [`append_charge`] — append a `charge` row recording the cash actually
//!     collected, called by the payment-confirmation webhook WITHOUT touching the
//!     finalized invoice. Mapping the Stripe invoice (`in_…`) back to the internal
//!     `zeroship.invoices.id` is [`invoice_id_for_provider_invoice`].
//!
//! Both helpers are generic over [`compio_postgres::GenericClient`] so they run on
//! a bare connection OR inside the webhook's transaction (the design requires the
//! charge row to be appended in the SAME tx that records the payment).

use compio_postgres::GenericClient;

use crate::registry::RegistryError;

/// Net cash the platform currently holds for `invoice_id`: `Σ(amount_cents)` over
/// its `invoice_payments` rows (positive `charge`/`dispute_reversal`, negative
/// `dispute_debit`). `0` when there are no rows (a fully credit-covered invoice).
///
/// This is the authoritative over-refund anchor — PR-3's over-refund trigger
/// inlines the identical SELECT so the DB-level backstop is self-contained, and
/// the Rust refund/true-up paths call THIS so they agree with the trigger.
pub async fn cash_collected<C: GenericClient + Sync>(
    conn: &C,
    invoice_id: &str,
) -> Result<i64, RegistryError> {
    // SUM(BIGINT) is NUMERIC in Postgres — cast back to BIGINT so compio-postgres
    // decodes it as i64. COALESCE(...,0) so a payment-less invoice reads 0.
    let rows = conn
        .query(
            "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS cash \
             FROM zeroship.invoice_payments WHERE invoice_id = $1",
            &[&invoice_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map(|r| r.get::<_, i64>("cash")).unwrap_or(0))
}

/// Resolve the internal `zeroship.invoices.id` for a finalized Stripe provider
/// invoice id (`in_…`) via `billing_provider_refs(provider='stripe',
/// ref_kind='invoice')`. `None` if no finalized invoice maps to it (e.g. a Connect
/// end-user invoice, or one the reconciler never finalized).
pub async fn invoice_id_for_provider_invoice<C: GenericClient + Sync>(
    conn: &C,
    provider_invoice_id: &str,
) -> Result<Option<String>, RegistryError> {
    let rows = conn
        .query(
            "SELECT invoice_id FROM zeroship.billing_provider_refs \
             WHERE provider = 'stripe' AND ref_kind = 'invoice' AND external_id = $1",
            &[&provider_invoice_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map(|r| r.get::<_, String>("invoice_id")))
}

/// Append a positive `charge` payment row recording the cash actually collected
/// against a finalized invoice. The finalized invoice row is NEVER touched — the
/// immutability trigger is never challenged (the whole point of the side table).
///
/// `amount_cents` is the cash collected (must be `> 0`; a $0 fully-credit-covered
/// invoice records NO row — the caller skips it, so cash-collected stays 0).
/// `provider_ref` is the Stripe event/object id (`in_…`/`pi_…`/`ch_…`) for audit.
///
/// Returns the new `ipy_…` id. Generic over the client so the webhook can call it
/// inside the same transaction that records the payment.
pub async fn append_charge<C: GenericClient + Sync>(
    conn: &C,
    invoice_id: &str,
    amount_cents: i64,
    currency: &str,
    provider_ref: Option<&str>,
) -> Result<String, RegistryError> {
    if amount_cents <= 0 {
        return Err(RegistryError::InvalidInput(format!(
            "invoice_payments charge amount must be > 0 (got {amount_cents}); \
             a $0 fully-credited invoice records no row"
        )));
    }
    let id = zeroship_core::typed_id::new_invoice_payment_id();
    conn.execute(
        "INSERT INTO zeroship.invoice_payments \
           (id, invoice_id, amount_cents, currency, kind, provider_ref) \
         VALUES ($1, $2, $3, $4, 'charge', $5)",
        &[&id, &invoice_id, &amount_cents, &currency, &provider_ref],
    )
    .await
    .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(id)
}
