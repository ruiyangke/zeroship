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

/// Persist the dispute-resolution linkage for a paid Stripe invoice (billing-ops gap #26,
/// PR-8 CRITICAL-1). A Stripe Dispute object carries NO `invoice` field — only `charge`
/// (`ch_…`) and `payment_intent` (`pi_…`). So to map a future `charge.dispute.*` back to
/// THIS internal invoice we must record, at `invoice.paid` time, the `pi_…`/`ch_…` that
/// settled the invoice as `billing_provider_refs` rows keyed by their own `ref_kind`
/// (`'payment_intent'` / `'charge'`) → the SAME `invoices(id)`.
///
/// A `pi_`/`ch_` is GLOBALLY unique at Stripe, so `billing_provider_refs`' existing
/// `UNIQUE(provider, ref_kind, external_id)` makes the later dispute resolution
/// deterministic (no ambiguity).
///
/// IDEMPOTENT + POISON-SAFE (C2). The table has TWO uniques: the PK
/// `(invoice_id, provider, ref_kind)` AND the global `(provider, ref_kind, external_id)`.
/// A bare `ON CONFLICT (invoice_id, provider, ref_kind) DO NOTHING` covers only the PK —
/// so a settling `pi_`/`ch_` that is ALREADY linked to invoice A and is then seen for a
/// DIFFERENT internal invoice B (Stripe reuses a `pi_` across a void+reissue, which mints
/// a new internal `invoice_id` while the same `pi_` settles) would violate the GLOBAL
/// unique, raise SQLSTATE 23505, propagate to the webhook (500), and POISON the event —
/// Stripe then retries forever, deterministically re-aborting.
///
/// A `pi_`/`ch_` legitimately maps to EXACTLY ONE settlement. When it is seen for a
/// second internal invoice it BELONGS TO THE FIRST: we therefore INSERT only when neither
/// unique is already satisfied (`WHERE NOT EXISTS` covers BOTH the PK and the global
/// tuple), and treat "already mapped — possibly under another invoice" as a benign,
/// idempotent no-op (logged). The webhook acks 200; no poison.
///
/// `payment_intent`/`charge` are each optional — a paid invoice carries one OR the other
/// (or both, across wire versions); whichever is present is persisted. Passing both `None`
/// is a harmless no-op (`Ok(())`). This does NOT touch the existing `'invoice'` (`in_…`)
/// ref or the `'charge'`-KIND `invoice_payments` row — it is an ADDITIONAL linkage, so
/// PR-1 charge idempotency and PR-3's invoice-ref guards are untouched.
pub async fn record_payment_object_refs<C: GenericClient + Sync>(
    conn: &C,
    invoice_id: &str,
    payment_intent: Option<&str>,
    charge: Option<&str>,
) -> Result<(), RegistryError> {
    for (ref_kind, external_id) in [("payment_intent", payment_intent), ("charge", charge)] {
        let Some(external_id) = external_id.map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        // INSERT only when this linkage does not already exist under ANY invoice for
        // this (provider, ref_kind, external_id) AND the PK row is absent. `WHERE NOT
        // EXISTS` covers BOTH uniques, so the INSERT can never trip the global-unique
        // 23505 that poisons the webhook. `RETURNING invoice_id` lets us detect the
        // "linkage already mapped elsewhere" case and log it (a void+reissue carried the
        // same pi_ over) instead of silently dropping a coherence signal.
        let inserted = conn
            .query(
                "INSERT INTO zeroship.billing_provider_refs \
                   (invoice_id, provider, ref_kind, external_id) \
                 SELECT $1, 'stripe', $2, $3 \
                 WHERE NOT EXISTS ( \
                     SELECT 1 FROM zeroship.billing_provider_refs \
                     WHERE provider = 'stripe' AND ref_kind = $2 AND external_id = $3 \
                 ) AND NOT EXISTS ( \
                     SELECT 1 FROM zeroship.billing_provider_refs \
                     WHERE invoice_id = $1 AND provider = 'stripe' AND ref_kind = $2 \
                 ) \
                 RETURNING invoice_id",
                &[&invoice_id, &ref_kind, &external_id],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        if inserted.is_empty() {
            // Either already linked to THIS invoice (plain redelivery — fine) or already
            // linked to ANOTHER invoice for this globally-unique pi_/ch_ (a void+reissue
            // that carried the same settling object over). The pi_/ch_ belongs to the
            // FIRST settlement; this is a benign idempotent no-op. Surface a debug line so
            // the (rare, legitimate) cross-invoice reuse is observable.
            tracing::debug!(
                invoice_id = %invoice_id,
                ref_kind = %ref_kind,
                "stripe: payment-object linkage already recorded (idempotent; possibly under another invoice) — ack"
            );
        }
    }
    Ok(())
}

/// Append a positive `charge` payment row recording the cash actually collected
/// against a finalized invoice. The finalized invoice row is NEVER touched — the
/// immutability trigger is never challenged (the whole point of the side table).
///
/// `amount_cents` is the **incremental** cash collected by THIS payment (must be
/// `> 0`; a $0 fully-credit-covered invoice records NO row — the caller skips it,
/// so cash-collected stays 0). A future partial-pay caller MUST pass the
/// INCREMENTAL amount of that one payment, **never** the Stripe invoice's
/// cumulative `amount_paid` — `cash_collected` is `Σ(amount_cents)`, so passing the
/// running total on each partial would double-count every prior payment.
///
/// `provider_ref` is the Stripe payment object id (`in_…`/`pi_…`/`ch_…`) and is the
/// IDEMPOTENCY KEY for `charge` rows. The append is `INSERT … ON CONFLICT DO
/// NOTHING` against the `(invoice_id, provider_ref) WHERE kind='charge'` partial
/// unique index (0046), so any number of webhook retries / Stripe redeliveries for
/// the SAME payment append EXACTLY ONE row — cash-collected (and PR-3's over-refund
/// cap that reads it) never over-counts. `provider_ref` MUST therefore be `Some`
/// for a charge; a `None` would defeat the dedup (and the index can't cover it).
///
/// Returns the `ipy_…` id of the charge row for this payment — the freshly inserted
/// id on first append, or the already-present row's id on a conflicting retry.
/// Generic over the client so the webhook can call it inside the same transaction
/// that records the payment.
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
    // ON CONFLICT DO NOTHING against the charge idempotency index makes a retry /
    // redelivery of the SAME Stripe payment a no-op. `RETURNING id` yields the new
    // id on insert and ZERO rows on conflict — in which case we read back the
    // already-present row's id so the caller always gets the canonical charge id.
    let inserted = conn
        .query(
            "INSERT INTO zeroship.invoice_payments \
               (id, invoice_id, amount_cents, currency, kind, provider_ref) \
             VALUES ($1, $2, $3, $4, 'charge', $5) \
             ON CONFLICT (invoice_id, provider_ref) WHERE kind = 'charge' \
             DO NOTHING \
             RETURNING id",
            &[&id, &invoice_id, &amount_cents, &currency, &provider_ref],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    if let Some(row) = inserted.first() {
        return Ok(row.get::<_, String>("id"));
    }
    // Conflict: a charge row for this (invoice_id, provider_ref) already exists.
    // Return its id (idempotent no-op append).
    let existing = conn
        .query(
            "SELECT id FROM zeroship.invoice_payments \
             WHERE invoice_id = $1 AND provider_ref = $2 AND kind = 'charge'",
            &[&invoice_id, &provider_ref],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    existing
        .first()
        .map(|r| r.get::<_, String>("id"))
        .ok_or_else(|| {
            RegistryError::Database(
                "append_charge ON CONFLICT but no existing charge row found".to_string(),
            )
        })
}

/// Append a signed dispute `invoice_payments` row (billing-ops gap #26, PR-8).
///
/// A `dispute_debit` (`amount_cents < 0`) records cash CLAWED BACK by a
/// `charge.dispute.created` — it LOWERS `Σ(invoice_payments)`, automatically tightening
/// PR-3's over-refund cap (which reads that sum) so a creator can't refund cash that was
/// charged back. A `dispute_reversal` (`amount_cents > 0`) records cash RESTORED by a
/// `charge.dispute.closed won`. Neither touches the finalized invoice — the immutability
/// trigger is never challenged.
///
/// IDEMPOTENCY (PR-8 do-not-regress): the append is `INSERT … ON CONFLICT DO NOTHING`
/// against the `(invoice_id, provider_ref, kind) WHERE kind IN
/// ('dispute_debit','dispute_reversal')` partial unique index (0053), so a REDELIVERED
/// dispute event under the same `du_…` `provider_ref` appends EXACTLY ONE row per
/// (dispute, kind) — never a double-debit / double-restore. `kind` is in the key so one
/// dispute's debit (created) and its later reversal (won) both fit. `provider_ref` (the
/// `du_…`) MUST be `Some` so the dedup index covers the row.
///
/// Returns the `ipy_…` id (fresh on insert, or the already-present row's id on a
/// conflicting redelivery). Generic over the client so the webhook can call it inside
/// the same transaction that records the `billing_disputes` row.
pub async fn append_dispute_row<C: GenericClient + Sync>(
    conn: &C,
    invoice_id: &str,
    amount_cents: i64,
    currency: &str,
    kind: DisputePaymentKind,
    provider_ref: &str,
) -> Result<String, RegistryError> {
    // Sign discipline: a debit MUST be negative (cash left), a reversal MUST be positive
    // (cash back). A zero row is meaningless (the table CHECK forbids it anyway).
    match kind {
        DisputePaymentKind::Debit if amount_cents >= 0 => {
            return Err(RegistryError::InvalidInput(format!(
                "dispute_debit amount must be < 0 (got {amount_cents}) — it claws cash back"
            )));
        }
        DisputePaymentKind::Reversal if amount_cents <= 0 => {
            return Err(RegistryError::InvalidInput(format!(
                "dispute_reversal amount must be > 0 (got {amount_cents}) — it restores cash"
            )));
        }
        _ => {}
    }
    if provider_ref.is_empty() {
        return Err(RegistryError::InvalidInput(
            "dispute payment row requires a non-empty provider_ref (the du_… dispute id)".to_string(),
        ));
    }
    let id = zeroship_core::typed_id::new_invoice_payment_id();
    let kind_str = kind.as_str();
    let inserted = conn
        .query(
            "INSERT INTO zeroship.invoice_payments \
               (id, invoice_id, amount_cents, currency, kind, provider_ref) \
             VALUES ($1, $2, $3, $4, $5::text::zeroship.invoice_payment_kind, $6) \
             ON CONFLICT (invoice_id, provider_ref, kind) \
               WHERE kind IN ('dispute_debit','dispute_reversal') \
             DO NOTHING \
             RETURNING id",
            &[&id, &invoice_id, &amount_cents, &currency, &kind_str, &provider_ref],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    if let Some(row) = inserted.first() {
        return Ok(row.get::<_, String>("id"));
    }
    // Conflict: a row for this (invoice_id, provider_ref, kind) already exists — return
    // its id (idempotent no-op append on a same-du_… redelivery).
    let existing = conn
        .query(
            "SELECT id FROM zeroship.invoice_payments \
             WHERE invoice_id = $1 AND provider_ref = $2 AND kind = $3::text::zeroship.invoice_payment_kind",
            &[&invoice_id, &provider_ref, &kind_str],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    existing
        .first()
        .map(|r| r.get::<_, String>("id"))
        .ok_or_else(|| {
            RegistryError::Database(
                "append_dispute_row ON CONFLICT but no existing dispute row found".to_string(),
            )
        })
}

/// Which signed dispute payment row to append. Maps to the `invoice_payment_kind`
/// domain values `dispute_debit` / `dispute_reversal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisputePaymentKind {
    /// `charge.dispute.created` clawback — a NEGATIVE row lowering cash_collected.
    Debit,
    /// `charge.dispute.closed won` restoration — a POSITIVE row raising cash_collected.
    Reversal,
}

impl DisputePaymentKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debit => "dispute_debit",
            Self::Reversal => "dispute_reversal",
        }
    }
}
