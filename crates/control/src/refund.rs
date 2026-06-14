//! Refunds — operator-only money-back against a finalized invoice (billing-ops gap
//! #26, PR-3; design `0049 refunds` + flow B "refund a finalized invoice").
//!
//! A refund is a SEPARATE append-only fact with a real FK to the PAID invoice — the
//! invoice itself is NEVER mutated (the immutability trigger would reject it anyway).
//! The money mechanism is split by destination, exactly as the design pins it against
//! the Stripe API:
//!
//!   * `destination='cash'` → a Stripe **`Refund` (`re_…`)** on the paid invoice's
//!     charge / PaymentIntent (verified at docs.stripe.com/api/refunds/create: a
//!     `Refund` returns funds to the original card; a credit note alone does NOT move
//!     cash on a paid invoice). The `re_…` is the authoritative money-movement ref.
//!   * `destination='credit'` → a platform-native `credit_ledger('refund_to_credit')`
//!     grant. NO Stripe charge reversal.
//!
//! ## The [`RefundProvider`] seam
//!
//! Mirroring [`crate::metering::provider::MeteringProvider`], the provider is the
//! pluggable "how the cash leg moves" layer ABOVE the low-level [`StripeApi`]:
//!
//!   * [`StripeRefundProvider`] (default in prod) issues the Stripe `Refund`.
//!   * [`NativeRefundProvider`] is the no-Stripe stub (the cash leg is a no-op that
//!     mints a synthetic ref) — for a USD/dev tier with no Stripe rail and for tests
//!     that don't exercise the wire.
//!
//! The `destination='credit'` leg never touches the provider — it is a pure
//! `credit_ledger` append, so the seam only abstracts the CASH call.
//!
//! ## Claim-then-call idempotency (mirrors the line provider-refs)
//!
//! 1. **Idempotency precheck + claim** (in [`issue_refund`]): `INSERT refunds (…,
//!    'pending', idempotency_key, request_fingerprint) ON CONFLICT (idempotency_key)
//!    DO NOTHING RETURNING`. 0 rows ⇒ a key hit ⇒ compare the stored fingerprint:
//!    matches → return the existing refund (safe retry); differs → 409 conflict. The
//!    over-refund trigger (`0049`) rejects the claim INSERT unless all three
//!    cash-anchored bounds hold (the Rust path also re-checks before claiming).
//! 2. **Call** the provider (cash) / append the `refund_to_credit` grant (credit).
//! 3. **Record claim-AFTER-success:** write the `refund_provider_refs` row (for cash)
//!    and flip `status='issued'`/`issued_at=NOW()`. A crash BEFORE this leaves the
//!    refund `pending` with no ref → a re-drive re-issues it (idempotent on the
//!    deterministic Stripe `Idempotency-Key`); a refund WITH a ref is skipped.

use compio_postgres::GenericClient;
use sha2::{Digest, Sha256};

use crate::credit;
use crate::registry::RegistryError;
use crate::stripe_client::StripeApi;
use crate::stripe_store::StripeError;

/// v1 is USD-pinned: every refund is USD (the invoice it refunds is USD).
pub const REFUND_CURRENCY: &str = "usd";

/// Where a refund's value goes: back to the card (`Cash`) or onto the creator's
/// credit balance (`Credit`). The string forms match the `refund_destination`
/// domain in `0049`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundDestination {
    Cash,
    Credit,
}

impl RefundDestination {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cash => "cash",
            Self::Credit => "credit",
        }
    }

    /// Parse the wire/body string (case-insensitive). Returns `None` for any value
    /// outside the `{cash, credit}` domain.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cash" => Some(Self::Cash),
            "credit" => Some(Self::Credit),
            _ => None,
        }
    }
}

/// The result of [`issue_refund`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefundOutcome {
    /// A fresh refund was issued; carries its `ref_…` id and (for cash) the `re_…`.
    Issued { refund_id: String, provider_ref: Option<String> },
    /// The idempotency key was reused with the SAME body — the first refund's id is
    /// returned (safe retry, no second refund).
    Duplicate(String),
    /// The idempotency key was reused with a DIFFERENT body — rejected (409). No
    /// refund was claimed.
    Conflict,
    /// The refund would exceed the cash actually collected on the invoice (the
    /// Σ(invoice_payments)-anchored over-refund cap). No refund was claimed.
    OverRefund(String),
    /// The invoice does not exist / is not finalized. No refund was claimed.
    InvalidInvoice(String),
}

/// SHA-256 over the canonical refund request — the body fingerprint a reused
/// idempotency key is compared against. A reused key with a DIFFERENT body
/// (amount / split / destination) is a 409, never a silent second refund. Mirrors
/// [`credit::grant_fingerprint`].
#[must_use]
pub fn refund_fingerprint(
    invoice_id: &str,
    amount_cents: i64,
    subtotal_cents: i64,
    tax_cents: i64,
    destination: RefundDestination,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(invoice_id.as_bytes());
    hasher.update(b"|");
    hasher.update(amount_cents.to_le_bytes());
    hasher.update(b"|");
    hasher.update(subtotal_cents.to_le_bytes());
    hasher.update(b"|");
    hasher.update(tax_cents.to_le_bytes());
    hasher.update(b"|");
    hasher.update(destination.as_str().as_bytes());
    hex::encode(hasher.finalize())
}

/// The deterministic Stripe `Idempotency-Key` for the cash `Refund` of `refund_id`.
/// Stable for a fixed refund row so a crash-retry replays the SAME `re_…`.
#[must_use]
pub fn stripe_refund_idempotency_key(refund_id: &str) -> String {
    format!("refund:{refund_id}")
}

/// The pluggable CASH-refund backend. Mirrors [`crate::metering::provider::MeteringProvider`]:
/// an object-safe `?Send` trait with a Native no-op default and a Stripe impl.
///
/// Only the CASH leg goes through here — `destination='credit'` is a pure
/// `credit_ledger` append, never a provider call.
#[async_trait::async_trait(?Send)]
pub trait RefundProvider {
    /// Move `amount_cents` of cash back to the card that paid `provider_invoice_id`
    /// (the Stripe invoice `in_…` recorded as the `charge` payment row's
    /// provider_ref). Returns the provider ref kind + external id (`('refund', re_…)`
    /// on Stripe). `idempotency_key` is the deterministic per-refund key.
    async fn refund_cash(
        &self,
        provider_invoice_id: &str,
        amount_cents: i64,
        currency: &str,
        idempotency_key: &str,
    ) -> Result<ProviderRefundRef, StripeError>;
}

/// A provider's cash-refund result: the ref kind (`'refund'` / `'credit_note'`) and
/// the external id (`re_…` / `cn_…`) to freeze into `refund_provider_refs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRefundRef {
    pub ref_kind: String,
    pub external_id: String,
}

/// The Stripe cash-refund provider: issues a `Refund` (`re_…`) via [`StripeApi`].
pub struct StripeRefundProvider<'a, S: StripeApi> {
    pub stripe: &'a S,
}

#[async_trait::async_trait(?Send)]
impl<S: StripeApi> RefundProvider for StripeRefundProvider<'_, S> {
    async fn refund_cash(
        &self,
        provider_invoice_id: &str,
        amount_cents: i64,
        currency: &str,
        idempotency_key: &str,
    ) -> Result<ProviderRefundRef, StripeError> {
        let amount = u64::try_from(amount_cents).map_err(|_| {
            StripeError::Validation(format!("refund amount must be >= 0 (got {amount_cents})"))
        })?;
        let re = self
            .stripe
            .create_refund(provider_invoice_id, amount, currency, idempotency_key)
            .await?;
        Ok(ProviderRefundRef { ref_kind: "refund".to_string(), external_id: re })
    }
}

/// The Native cash-refund provider: NO Stripe rail (a USD/dev tier with no provider,
/// or a test that doesn't exercise the wire). It mints a synthetic, deterministic
/// `re_native_<idem>` ref so the claim-after-success bookkeeping is uniform with the
/// Stripe path — but no cash actually moves at a provider. Mirrors
/// [`crate::metering::provider::native::NativeProvider`]'s no-op posture.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeRefundProvider;

#[async_trait::async_trait(?Send)]
impl RefundProvider for NativeRefundProvider {
    async fn refund_cash(
        &self,
        _provider_invoice_id: &str,
        _amount_cents: i64,
        _currency: &str,
        idempotency_key: &str,
    ) -> Result<ProviderRefundRef, StripeError> {
        Ok(ProviderRefundRef {
            ref_kind: "refund".to_string(),
            external_id: format!("re_native_{idempotency_key}"),
        })
    }
}

/// Resolve the Stripe invoice id (`in_…`) recorded on a `charge` payment row for
/// this invoice — the money object a cash refund targets. Returns `None` if the
/// invoice has no recorded charge (a fully-credit-covered $0 invoice, or a bill not
/// yet paid) — in which case there is no cash to refund anyway (the over-refund cap
/// = 0 already rejects it).
pub async fn provider_invoice_id_for_cash_refund<C: GenericClient + Sync>(
    conn: &C,
    invoice_id: &str,
) -> Result<Option<String>, RegistryError> {
    let rows = conn
        .query(
            "SELECT provider_ref FROM zeroship.invoice_payments \
             WHERE invoice_id = $1 AND kind = 'charge' AND provider_ref IS NOT NULL \
             ORDER BY created_at LIMIT 1",
            &[&invoice_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().and_then(|r| r.get::<_, Option<String>>("provider_ref")))
}

/// The net cash collected on an invoice: `Σ(invoice_payments.amount_cents)`. The
/// over-refund anchor (NOT `total_cents`).
pub async fn cash_collected<C: GenericClient + Sync>(
    conn: &C,
    invoice_id: &str,
) -> Result<i64, RegistryError> {
    let rows = conn
        .query(
            "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS cash \
             FROM zeroship.invoice_payments WHERE invoice_id = $1",
            &[&invoice_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map_or(0, |r| r.get::<_, i64>("cash")))
}

/// Σ of refunds already issued/claimed on an invoice for one destination — used by
/// the Rust-side over-refund precheck (the DB trigger is the backstop) and the
/// true-up bridge's `cash_refunds_already_issued`.
pub async fn refunds_total_for_destination<C: GenericClient + Sync>(
    conn: &C,
    invoice_id: &str,
    destination: RefundDestination,
) -> Result<i64, RegistryError> {
    let rows = conn
        .query(
            "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS s \
             FROM zeroship.refunds WHERE invoice_id = $1 AND destination = $2::text",
            &[&invoice_id, &destination.as_str()],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map_or(0, |r| r.get::<_, i64>("s")))
}

/// Issue a refund against a finalized invoice — the operator flow's core helper.
///
/// `conn` MUST be a live connection (a fresh `conn.transaction()` is taken
/// internally for the claim, and the provider call happens OUTSIDE any txn — a
/// network call must never hold a DB txn open). Generic over the [`StripeApi`] so a
/// test drives a recording fake.
///
/// Steps (claim-then-call):
///   1. Validate the invoice is finalized + read its creator_id/currency.
///   2. Rust-side over-refund precheck against `Σ(invoice_payments)` (the DB trigger
///      is the backstop).
///   3. Claim the `refunds` row `ON CONFLICT (idempotency_key) DO NOTHING` →
///      Duplicate/Conflict on a key hit (fingerprint compare); OverRefund if the
///      trigger rejects.
///   4. Call the provider (cash) / append the `refund_to_credit` grant (credit).
///   5. Record claim-after-success: the provider ref + `status='issued'`.
#[allow(clippy::too_many_arguments)]
pub async fn issue_refund<C: GenericClient + Sync, P: RefundProvider>(
    conn: &C,
    provider: &P,
    invoice_id: &str,
    amount_cents: i64,
    subtotal_cents: i64,
    tax_cents: i64,
    destination: RefundDestination,
    reason: Option<&str>,
    idempotency_key: &str,
) -> Result<RefundOutcome, RegistryError> {
    issue_refund_inner(
        conn, provider, invoice_id, amount_cents, subtotal_cents, tax_cents, destination, reason,
        idempotency_key, false,
    )
    .await
}

/// The true-up bridge variant: refund the over-collection on a now-VOIDED invoice.
/// Identical to [`issue_refund`] except it ACCEPTS a `void` invoice (the over-refund
/// cap reads `Σ(invoice_payments)`, which survives the void, so the refund is still
/// money-correct). Used ONLY by [`crate::void_reissue`]; the public operator endpoint
/// always goes through [`issue_refund`] (finalized-only).
#[allow(clippy::too_many_arguments)]
pub async fn issue_true_up_refund<C: GenericClient + Sync, P: RefundProvider>(
    conn: &C,
    provider: &P,
    invoice_id: &str,
    amount_cents: i64,
    subtotal_cents: i64,
    tax_cents: i64,
    destination: RefundDestination,
    reason: Option<&str>,
    idempotency_key: &str,
) -> Result<RefundOutcome, RegistryError> {
    issue_refund_inner(
        conn, provider, invoice_id, amount_cents, subtotal_cents, tax_cents, destination, reason,
        idempotency_key, true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn issue_refund_inner<C: GenericClient + Sync, P: RefundProvider>(
    conn: &C,
    provider: &P,
    invoice_id: &str,
    amount_cents: i64,
    subtotal_cents: i64,
    tax_cents: i64,
    destination: RefundDestination,
    reason: Option<&str>,
    idempotency_key: &str,
    allow_voided: bool,
) -> Result<RefundOutcome, RegistryError> {
    if amount_cents <= 0 {
        return Err(RegistryError::InvalidInput(format!(
            "refund amount must be > 0 (got {amount_cents})"
        )));
    }
    if subtotal_cents < 0 || tax_cents < 0 || subtotal_cents + tax_cents != amount_cents {
        return Err(RegistryError::InvalidInput(format!(
            "refund split invalid: amount {amount_cents} must equal subtotal {subtotal_cents} + tax {tax_cents}"
        )));
    }
    if idempotency_key.is_empty() {
        return Err(RegistryError::InvalidInput(
            "refund requires a non-empty Idempotency-Key".to_string(),
        ));
    }

    // (1) The invoice must exist and be finalized. A void/draft invoice is not
    // refundable. Read its currency + creator for the credit-destination grant.
    let inv = conn
        .query(
            "SELECT creator_id, currency, status FROM zeroship.invoices WHERE id = $1",
            &[&invoice_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let Some(row) = inv.first() else {
        return Ok(RefundOutcome::InvalidInvoice(format!("no invoice {invoice_id}")));
    };
    let currency: String = row.get("currency");
    let status: String = row.get("status");
    // A finalized invoice is refundable. The true-up bridge (`allow_voided`) also
    // refunds a deliberately-VOIDED invoice's over-collection — the cap reads
    // Σ(invoice_payments), which survives the void, so it stays money-correct. A draft
    // invoice is never refundable. (The creator_id for a credit-destination grant is
    // re-read inside `drive_pending_refund` via the refunds→invoices join.)
    let refundable = status == "finalized" || (allow_voided && status == "void");
    if !refundable {
        return Ok(RefundOutcome::InvalidInvoice(format!(
            "invoice {invoice_id} is {status} — not refundable"
        )));
    }

    let fingerprint =
        refund_fingerprint(invoice_id, amount_cents, subtotal_cents, tax_cents, destination);

    // (2) Rust-side over-refund precheck against Σ(invoice_payments). The DB trigger
    // is the authoritative backstop; this gives a clean OverRefund outcome (vs a raw
    // CHECK error) before we mint an id.
    let cash = cash_collected(conn, invoice_id).await?;
    let prior_cash = refunds_total_for_destination(conn, invoice_id, RefundDestination::Cash).await?;
    let prior_credit =
        refunds_total_for_destination(conn, invoice_id, RefundDestination::Credit).await?;
    let (new_cash, new_credit) = match destination {
        RefundDestination::Cash => (prior_cash + amount_cents, prior_credit),
        RefundDestination::Credit => (prior_cash, prior_credit + amount_cents),
    };
    if new_cash > cash || new_credit > cash || new_cash + new_credit > cash {
        return Ok(RefundOutcome::OverRefund(format!(
            "refund of {amount_cents} ({}) would exceed cash collected {cash} on invoice {invoice_id} \
             (existing cash {prior_cash} / credit {prior_credit})",
            destination.as_str()
        )));
    }

    // (3) Claim. INSERT … ON CONFLICT (idempotency_key) DO NOTHING. The over-refund
    // trigger fires on this INSERT; if it RAISEs (a concurrent claim pushed past the
    // cap between our precheck and now) we surface OverRefund. We do the claim in its
    // OWN short txn so a trigger RAISE doesn't poison the caller's connection state.
    let refund_id = zeroship_core::typed_id::new_refund_id();
    let claim = conn
        .query(
            "INSERT INTO zeroship.refunds \
               (id, invoice_id, amount_cents, subtotal_cents, tax_cents, currency, \
                destination, reason, idempotency_key, request_fingerprint, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7::text, $8, $9, $10, 'pending') \
             ON CONFLICT (idempotency_key) DO NOTHING \
             RETURNING id",
            &[
                &refund_id,
                &invoice_id,
                &amount_cents,
                &subtotal_cents,
                &tax_cents,
                &currency,
                &destination.as_str(),
                &reason,
                &idempotency_key,
                &fingerprint,
            ],
        )
        .await;
    let claim = match claim {
        Ok(rows) => rows,
        Err(e) => {
            // The over-refund trigger RAISEs as a raised_exception (P0001). Map it to
            // a clean OverRefund; any other DB error propagates.
            if e.code() == Some(&compio_postgres::error::SqlState::RAISE_EXCEPTION) {
                return Ok(RefundOutcome::OverRefund(format!(
                    "over-refund trigger rejected the claim on invoice {invoice_id}"
                )));
            }
            return Err(RegistryError::Database(e.to_string()));
        }
    };

    if claim.first().is_none() {
        // Key hit — compare the stored fingerprint (safe retry vs reuse-conflict).
        let existing = conn
            .query(
                "SELECT id, request_fingerprint FROM zeroship.refunds WHERE idempotency_key = $1",
                &[&idempotency_key],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        let Some(er) = existing.first() else {
            return Ok(RefundOutcome::Conflict);
        };
        let existing_id: String = er.get("id");
        let existing_fp: String = er.get("request_fingerprint");
        if existing_fp == fingerprint {
            // Same key + same body. The first refund may still be 'pending' (a prior
            // crash before the provider call) — re-drive it to converge. Re-driving a
            // refund WITH a ref is a no-op skip.
            return drive_pending_refund(conn, provider, &existing_id).await;
        }
        return Ok(RefundOutcome::Conflict);
    }

    // (4) + (5): the row is claimed ('pending'). Drive it to 'issued'.
    drive_pending_refund(conn, provider, &refund_id).await
}

/// Drive a claimed `pending` refund to `issued`: call the provider (cash) / append
/// the `refund_to_credit` grant (credit), then record the provider ref + flip the
/// status. Idempotent: a refund already `issued` (or with a recorded ref) is a skip.
///
/// This is the re-drive entry point — a crash between claim and provider-call leaves
/// a `pending` row that a retried operator call (same key + body) re-drives here.
async fn drive_pending_refund<C: GenericClient + Sync, P: RefundProvider>(
    conn: &C,
    provider: &P,
    refund_id: &str,
) -> Result<RefundOutcome, RegistryError> {
    let row = conn
        .query(
            "SELECT r.invoice_id, r.amount_cents, r.currency, r.destination::text AS destination, \
                    r.status, i.creator_id, \
                    pr.external_id AS existing_ref \
             FROM zeroship.refunds r \
             JOIN zeroship.invoices i ON i.id = r.invoice_id \
             LEFT JOIN zeroship.refund_provider_refs pr \
                    ON pr.refund_id = r.id AND pr.provider = 'stripe' AND pr.ref_kind = 'refund' \
             WHERE r.id = $1",
            &[&refund_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let Some(r) = row.first() else {
        return Err(RegistryError::Database(format!("refund {refund_id} vanished after claim")));
    };
    let invoice_id: String = r.get("invoice_id");
    let amount_cents: i64 = r.get("amount_cents");
    let currency: String = r.get("currency");
    let destination: String = r.get("destination");
    let status: String = r.get("status");
    let creator_id: uuid::Uuid = r.get("creator_id");
    let existing_ref: Option<String> = r.get("existing_ref");

    if status == "issued" && (destination == "credit" || existing_ref.is_some()) {
        // Already fully issued — a safe-retry no-op. Return the recorded ref.
        return Ok(RefundOutcome::Duplicate(refund_id.to_string()));
    }

    let destination_enum = RefundDestination::parse(&destination)
        .ok_or_else(|| RegistryError::Database(format!("bad refund destination {destination:?}")))?;

    match destination_enum {
        RefundDestination::Cash => {
            // If a ref already exists (crash after provider call, before status flip),
            // skip the provider call and just converge the status.
            let provider_ref = if let Some(re) = existing_ref {
                re
            } else {
                let pinv = provider_invoice_id_for_cash_refund(conn, &invoice_id)
                    .await?
                    .ok_or_else(|| {
                        RegistryError::InvalidInput(format!(
                            "invoice {invoice_id} has no recorded charge — nothing to cash-refund"
                        ))
                    })?;
                let idem = stripe_refund_idempotency_key(refund_id);
                let pref = provider
                    .refund_cash(&pinv, amount_cents, &currency, &idem)
                    .await
                    .map_err(|e| RegistryError::Database(format!("refund provider call: {e}")))?;
                // Record claim-AFTER-success: the provider ref first.
                conn.execute(
                    "INSERT INTO zeroship.refund_provider_refs \
                       (refund_id, provider, ref_kind, external_id) \
                     VALUES ($1, 'stripe', $2, $3) \
                     ON CONFLICT (refund_id, provider, ref_kind) DO NOTHING",
                    &[&refund_id, &pref.ref_kind, &pref.external_id],
                )
                .await
                .map_err(|e| RegistryError::Database(e.to_string()))?;
                pref.external_id
            };
            conn.execute(
                "UPDATE zeroship.refunds SET status = 'issued', issued_at = NOW() \
                 WHERE id = $1 AND status = 'pending'",
                &[&refund_id],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
            Ok(RefundOutcome::Issued {
                refund_id: refund_id.to_string(),
                provider_ref: Some(provider_ref),
            })
        }
        RefundDestination::Credit => {
            // Platform-native: append a `refund_to_credit` grant, NO Stripe call. The
            // grant is keyed off the refund id (one grant per refund) so a re-drive
            // does not double-grant: guard on an existing grant for this invoice with
            // a matching note marker.
            let marker = refund_to_credit_note(refund_id);
            let existing_grant = conn
                .query(
                    "SELECT 1 FROM zeroship.credit_ledger \
                     WHERE kind = 'refund_to_credit' AND applied_invoice_id = $1 AND note = $2",
                    &[&invoice_id, &marker],
                )
                .await
                .map_err(|e| RegistryError::Database(e.to_string()))?;
            if existing_grant.first().is_none() {
                let grant_id = zeroship_core::typed_id::new_credit_id();
                conn.execute(
                    "INSERT INTO zeroship.credit_ledger \
                       (id, creator_id, kind, amount_cents, currency, applied_invoice_id, note) \
                     VALUES ($1, $2, 'refund_to_credit', $3, $4, $5, $6)",
                    &[&grant_id, &creator_id, &amount_cents, &currency, &invoice_id, &marker],
                )
                .await
                .map_err(|e| RegistryError::Database(e.to_string()))?;
            }
            conn.execute(
                "UPDATE zeroship.refunds SET status = 'issued', issued_at = NOW() \
                 WHERE id = $1 AND status = 'pending'",
                &[&refund_id],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
            Ok(RefundOutcome::Issued { refund_id: refund_id.to_string(), provider_ref: None })
        }
    }
}

/// The stable `credit_ledger.note` marker for the `refund_to_credit` grant a
/// `destination='credit'` refund appends — the re-drive dedup key (one grant per
/// refund, even though `credit_ledger` carries no refund FK).
#[must_use]
pub fn refund_to_credit_note(refund_id: &str) -> String {
    format!("refund_to_credit:{refund_id}")
}

/// Re-export so callers don't need to thread [`credit`] for the credit-back path.
pub use credit::CREDIT_CURRENCY;
