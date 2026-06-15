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
//! 1. **Idempotency precheck + claim** (in [`claim_refund_locked`], run inside a
//!    transaction that takes the per-creator advisory lock as its FIRST act —
//!    mirroring [`crate::credit::consume_at_finalize`]): `INSERT refunds (…, 'pending',
//!    idempotency_key, request_fingerprint) ON CONFLICT (idempotency_key) DO NOTHING
//!    RETURNING`. 0 rows ⇒ a key hit ⇒ compare the stored fingerprint: matches → re-drive
//!    the existing refund (safe retry); differs → 409 conflict. The over-refund trigger
//!    (`0049`) rejects the claim INSERT unless all three cash-anchored bounds hold (the
//!    Rust path also re-checks before claiming).
//! 2. **Call** the provider (cash) / append the `refund_to_credit` grant (credit).
//! 3. **Record claim-AFTER-success:** write the `refund_provider_refs` row (for cash)
//!    and flip `status='issued'`/`issued_at=NOW()`. A crash BEFORE this leaves the
//!    refund `pending` with no ref → a re-drive re-issues it (idempotent on the
//!    deterministic Stripe `Idempotency-Key`); a refund WITH a ref is skipped.
//!
//! ## The per-creator advisory lock (over-refund correctness under concurrency)
//!
//! The over-refund cap is enforced by both the Rust precheck and the `0049`
//! BEFORE-INSERT trigger, each reading `Σ(invoice_payments)` against `Σ(refunds)`. Under
//! READ COMMITTED neither can see a CONCURRENT, still-uncommitted sibling refund: two
//! simultaneous refunds could each pass the 3-way bound and together exceed
//! `cash_collected`. So the precheck + claim INSERT run inside a transaction whose FIRST
//! statement is `pg_advisory_xact_lock(hashtext(creator_id::text)::bigint)` — the SAME
//! per-creator key [`crate::credit::consume_at_finalize`] takes. Two refunds for one
//! creator therefore SERIALIZE: the second's precheck/claim sees the first's committed
//! `pending` row and is rejected. The trigger is a single-statement BACKSTOP; the
//! application-side lock is what makes the bound hold under concurrency. The provider
//! (network) call runs AFTER the claim txn commits — a network call never holds a DB txn
//! (or the lock) open.

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

/// Resolve the Stripe MONEY OBJECT a cash refund targets for this invoice. Returns
/// `None` if the invoice has no recorded charge (a fully-credit-covered $0 invoice, or
/// a bill not yet paid) — in which case there is no cash to refund anyway (the
/// over-refund cap = 0 already rejects it).
///
/// PREFERENCE ORDER (D2): the settling `payment_intent` (`pi_…`) recorded in
/// `billing_provider_refs` at `invoice.paid` time — captured from the REAL expanded
/// invoice fetch — is the authoritative refund target and is preferred. We fall back to
/// the recorded `charge` (`ch_…`), then to the `charge` payment row's `provider_ref`
/// (the Stripe `in_…` invoice id). `create_refund` accepts whichever kind: a `pi_…`/
/// `ch_…` is refunded DIRECTLY; an `in_…` is resolved to its settling PaymentIntent via
/// an expanded fetch. Preferring the recorded `pi_…` means a refund works even when the
/// Stripe invoice object itself does not inline the settlement (e.g. an out-of-band
/// payment), and avoids a redundant invoice fetch on the hot path.
pub async fn provider_invoice_id_for_cash_refund<C: GenericClient + Sync>(
    conn: &C,
    invoice_id: &str,
) -> Result<Option<String>, RegistryError> {
    // 1) The settling pi_/ch_ recorded at invoice.paid (the real money object).
    let recorded = conn
        .query(
            "SELECT ref_kind, external_id FROM zeroship.billing_provider_refs \
             WHERE invoice_id = $1 AND provider = 'stripe' \
               AND ref_kind IN ('payment_intent', 'charge')",
            &[&invoice_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let mut pi: Option<String> = None;
    let mut ch: Option<String> = None;
    for row in &recorded {
        match row.get::<_, String>("ref_kind").as_str() {
            "payment_intent" => pi = Some(row.get("external_id")),
            "charge" => ch = Some(row.get("external_id")),
            _ => {}
        }
    }
    if let Some(target) = pi.or(ch) {
        return Ok(Some(target));
    }
    // 2) Fall back to the charge row's provider_ref (the Stripe `in_…` invoice id).
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

/// The net cash collected on an invoice: `Σ(invoice_payments.amount_cents)` — the
/// over-refund anchor (NOT `total_cents`). Re-exported from [`crate::invoice_payments`] so
/// the cap anchor has EXACTLY ONE implementation (M1): the refund precheck, the true-up
/// bridge, and the `0049` over-refund trigger all read the identical `Σ`. The historical
/// `refund::cash_collected` path is preserved for callers; there is no second copy to drift.
pub use crate::invoice_payments::cash_collected;

/// Σ of refunds already issued/claimed on an invoice for one destination — used by
/// the Rust-side over-refund precheck (the DB trigger is the backstop) and the
/// true-up bridge's `cash_refunds_already_issued`.
///
/// EXCLUDES `failed`/`canceled` refunds (0054): a refund the bank later rejected
/// returned NO cash, so it must not consume the over-refund cap — otherwise a creator
/// whose refund bounced could never re-refund. This filter mirrors the
/// `refunds_no_over_refund` trigger (which applies the same `status NOT IN
/// ('failed','canceled')` filter) so the Rust precheck and the DB backstop agree.
pub async fn refunds_total_for_destination<C: GenericClient + Sync>(
    conn: &C,
    invoice_id: &str,
    destination: RefundDestination,
) -> Result<i64, RegistryError> {
    let rows = conn
        .query(
            "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS s \
             FROM zeroship.refunds \
             WHERE invoice_id = $1 AND destination = $2::text \
               AND status NOT IN ('failed','canceled')",
            &[&invoice_id, &destination.as_str()],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map_or(0, |r| r.get::<_, i64>("s")))
}

/// The stable `credit_ledger.note` marker for the `refund_clawback` entry that offsets a
/// FAILED `destination='credit'` refund's `refund_to_credit` grant (0054). One clawback per
/// refund; the `credit_ledger_refund_clawback_note_idx` partial UNIQUE over it makes a
/// duplicate clawback a DB impossibility, so a redelivered `charge.refund.updated` can never
/// double-claw.
#[must_use]
pub fn refund_clawback_note(refund_id: &str) -> String {
    format!("refund_clawback:{refund_id}")
}

/// The terminal outcome of a `charge.refund.updated` reconciliation (the webhook leg).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefundFailureOutcome {
    /// The refund was reconciled to `failed`/`canceled` for the FIRST time. Carries the
    /// `ref_…` id and, for a `destination='credit'` refund, whether a `refund_clawback`
    /// entry was appended (the grant clawed back).
    Reversed { refund_id: String, clawed_back_credit: bool },
    /// The refund row was ALREADY terminal (`failed`/`canceled`) — a redelivered
    /// `charge.refund.updated` is a no-op (no double-reversal).
    AlreadyReversed(String),
    /// No `refunds` row resolves from the `re_…` (a Connect / non-platform refund, or a
    /// pre-issue race) — nothing to reconcile; the webhook acks.
    Unknown,
}

/// Reconcile a Stripe `charge.refund.updated` whose Refund (`re_…`) went terminal-FAILED
/// or -CANCELED (the cash did NOT return to the cardholder). MONEY-CRITICAL, idempotent.
///
/// Resolves OUR refund row through `refund_provider_refs(ref_kind='refund', external_id=
/// re_…)` (the cash ref recorded claim-after-success at issue time), then in ONE txn:
///   1. flips `refunds.status` → the terminal failure (gated `WHERE status NOT IN
///      ('failed','canceled')`, so a redelivery is a 0-row no-op) and stamps `failed_at`.
///      This makes the over-refund cap STOP counting the refund (the trigger + the Rust
///      precheck exclude failed/canceled) — so a failed CASH refund no longer permanently
///      reduces refundable cash; the creator CAN re-refund.
///   2. for a `destination='credit'` refund, appends a compensating NEGATIVE
///      `credit_ledger('refund_clawback')` entry that offsets the `refund_to_credit` grant
///      (the cash never left Stripe, so the creator must not keep the minted credit). The
///      clawback NAMES the original grant via `consumed_from_grant_id` and is idempotent on
///      the `refund_clawback:<refund_id>` note (the partial UNIQUE makes a double-claw
///      impossible). Balance is conserved: grant (+amount) + clawback (−amount) = 0.
///
/// `terminal` MUST be `failed` or `canceled`. `conn` must be a live OWNED connection
/// (`&mut`) — the flip + the clawback append run in one transaction so they land atomically.
pub async fn reconcile_failed_refund<C: GenericClient + Sync>(
    conn: &mut C,
    provider_refund_id: &str,
    terminal: &str,
) -> Result<RefundFailureOutcome, RegistryError> {
    if !matches!(terminal, "failed" | "canceled") {
        return Err(RegistryError::InvalidInput(format!(
            "reconcile_failed_refund expects 'failed'|'canceled' (got {terminal:?})"
        )));
    }

    let tx = conn
        .transaction()
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    // Resolve our refund row from the re_… cash ref, joining the invoice for the creator
    // (the clawback needs creator_id) and the grant we may need to claw back.
    let resolved = tx
        .query(
            "SELECT r.id, r.destination::text AS destination, r.amount_cents, r.currency, \
                    r.status::text AS status, r.invoice_id, i.creator_id \
             FROM zeroship.refund_provider_refs pr \
             JOIN zeroship.refunds r ON r.id = pr.refund_id \
             JOIN zeroship.invoices i ON i.id = r.invoice_id \
             WHERE pr.provider = 'stripe' AND pr.ref_kind = 'refund' AND pr.external_id = $1",
            &[&provider_refund_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let Some(row) = resolved.first() else {
        // No platform refund maps to this re_… (a Connect / out-of-band refund). Ack.
        tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        return Ok(RefundFailureOutcome::Unknown);
    };
    let refund_id: String = row.get("id");
    let destination: String = row.get("destination");
    let amount_cents: i64 = row.get("amount_cents");
    let currency: String = row.get("currency");
    let status: String = row.get("status");
    let invoice_id: String = row.get("invoice_id");
    let creator_id: uuid::Uuid = row.get("creator_id");

    if status == "failed" || status == "canceled" {
        // Already reconciled — a redelivered charge.refund.updated. No double-reversal.
        tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        return Ok(RefundFailureOutcome::AlreadyReversed(refund_id));
    }

    // (1) Flip to the terminal failure. Gated so a concurrent redelivery loses the race and
    // the second flip is a 0-row no-op (idempotent). The over-refund cap stops counting it.
    let flipped = tx
        .query(
            "UPDATE zeroship.refunds \
                SET status = $2::text::zeroship.refund_status, failed_at = NOW() \
              WHERE id = $1 AND status NOT IN ('failed','canceled') \
              RETURNING id",
            &[&refund_id, &terminal],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    if flipped.first().is_none() {
        // Lost a concurrent race to another reconciliation — it already reversed.
        tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        return Ok(RefundFailureOutcome::AlreadyReversed(refund_id));
    }

    // (2) For a CREDIT-destination refund, claw back the minted refund_to_credit grant.
    let mut clawed_back_credit = false;
    if destination == "credit" {
        // The grant this refund minted (refund_to_credit:<refund_id>). Resolve its id so the
        // clawback can name it via consumed_from_grant_id (the kind↔grant-ref CHECK requires it).
        let grant_marker = refund_to_credit_note(&refund_id);
        let grant_rows = tx
            .query(
                "SELECT id FROM zeroship.credit_ledger \
                 WHERE kind = 'refund_to_credit' AND note = $1",
                &[&grant_marker],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        if let Some(g) = grant_rows.first() {
            let grant_id: String = g.get("id");
            let clawback_marker = refund_clawback_note(&refund_id);
            let clawback_id = zeroship_core::typed_id::new_credit_id();
            // NEGATIVE amount (the kind↔sign CHECK requires refund_clawback < 0). ON CONFLICT
            // DO NOTHING on the per-refund note ⇒ a redelivered failure never double-claws.
            let inserted = tx
                .query(
                    "INSERT INTO zeroship.credit_ledger \
                       (id, creator_id, kind, amount_cents, currency, applied_invoice_id, \
                        consumed_from_grant_id, note) \
                     VALUES ($1, $2, 'refund_clawback', $3, $4, $5, $6, $7) \
                     ON CONFLICT (note) WHERE kind = 'refund_clawback' DO NOTHING \
                     RETURNING id",
                    &[
                        &clawback_id,
                        &creator_id,
                        &(-amount_cents),
                        &currency,
                        &invoice_id,
                        &grant_id,
                        &clawback_marker,
                    ],
                )
                .await
                .map_err(|e| RegistryError::Database(e.to_string()))?;
            clawed_back_credit = inserted.first().is_some();
        }
    }

    tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(RefundFailureOutcome::Reversed { refund_id, clawed_back_credit })
}

/// Map a Stripe Refund `status` (`charge.refund.updated`) onto OUR terminal-failure string.
/// Returns `Some("failed")` / `Some("canceled")` for the two cash-did-not-return states, and
/// `None` for `succeeded`/`pending`/`requires_action` (no reconciliation needed — the refund
/// is fine / still in flight). Verified against docs.stripe.com/api/refunds/object: status ∈
/// {pending, requires_action, succeeded, failed, canceled}.
#[must_use]
pub fn stripe_refund_failure_status(stripe_status: &str) -> Option<&'static str> {
    match stripe_status {
        "failed" => Some("failed"),
        "canceled" => Some("canceled"),
        _ => None,
    }
}

/// Issue a refund against a finalized invoice — the operator flow's core helper.
///
/// `conn` MUST be a live, OWNED connection (`&mut`): the precheck + claim INSERT run
/// inside a `conn.transaction()` whose first act is the per-creator advisory lock
/// (mirroring [`crate::credit::consume_at_finalize`]), so the over-refund bound holds
/// under concurrency. The provider (network) call happens AFTER that txn commits —
/// a network call must never hold a DB txn (or the lock) open. Generic over the
/// [`StripeApi`] so a test drives a recording fake.
///
/// Steps (claim-then-call):
///   1. Validate the invoice is finalized + read its creator_id/currency.
///   2. Open a txn, take `pg_advisory_xact_lock(creator)`, run the Rust-side
///      over-refund precheck against `Σ(invoice_payments)` (the DB trigger is the
///      backstop) and claim the `refunds` row `ON CONFLICT (idempotency_key) DO NOTHING`
///      → Duplicate/Conflict on a key hit (fingerprint compare); OverRefund if the
///      trigger rejects. Commit the claim txn (releasing the lock).
///   3. Call the provider (cash) / append the `refund_to_credit` grant (credit).
///   4. Record claim-after-success: the provider ref + `status='issued'`.
#[allow(clippy::too_many_arguments)]
pub async fn issue_refund<C: GenericClient + Sync, P: RefundProvider>(
    conn: &mut C,
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
///
/// ACCEPTS a `void` invoice (the over-refund cap reads `Σ(invoice_payments)`, which survives
/// the void, so the refund is still money-correct). Used ONLY by [`crate::void_reissue`]; the
/// public operator endpoint always goes through [`issue_refund`] (finalized-only).
///
/// STALE-WINDOW SAFE (H1). The over-collection
/// `over = cash_collected(old) − cash_refunds_already(old) − reissued_total`, floored at 0,
/// is recomputed INSIDE the same per-creator-locked txn that claims the refund — NOT read by
/// the caller before the lock. Caller-side computation opened a window where a concurrent
/// refund/dispute landing between the read and the claim's lock re-acquire made `over` stale:
/// the claim would then either trip the over-refund trigger (the "impossible by construction"
/// abort) or under-refund. By taking the lock first and reading the cash anchor under it, the
/// claimed amount is always consistent with the committed cash state. `reissued_total` is the
/// only input — it is the immutable reissued invoice's total, which cannot drift.
///
/// Returns `RefundOutcome::Duplicate` with a sentinel id when the recomputed over-collection
/// is `≤ 0` (nothing to refund — e.g. a concurrent refund already returned the over-collection):
/// the void+reissue then reports no true-up, which is correct.
pub async fn issue_true_up_refund<C: GenericClient + Sync, P: RefundProvider>(
    conn: &mut C,
    provider: &P,
    invoice_id: &str,
    reissued_total: i64,
    reason: Option<&str>,
    idempotency_key: &str,
) -> Result<RefundOutcome, RegistryError> {
    if idempotency_key.is_empty() {
        return Err(RegistryError::InvalidInput(
            "true-up refund requires a non-empty Idempotency-Key".to_string(),
        ));
    }

    // (1) The voided invoice must exist; read its creator (keys the lock) + currency.
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
    let creator_id: uuid::Uuid = row.get("creator_id");
    let currency: String = row.get("currency");
    let status: String = row.get("status");
    // The true-up runs against the deliberately-VOIDED invoice (or, defensively, a still
    // finalized one if called before the void committed). A draft is never refundable.
    if status != "void" && status != "finalized" {
        return Ok(RefundOutcome::InvalidInvoice(format!(
            "invoice {invoice_id} is {status} — not true-up-refundable"
        )));
    }

    // (2)+(3) Recompute the over-collection AND claim, both under the per-creator lock in ONE
    // txn — no stale window (H1). The claim commits before the provider call.
    let claim = {
        let tx = conn.transaction().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        let claim = claim_true_up_locked(
            &tx, &creator_id, invoice_id, reissued_total, &currency, reason, idempotency_key,
        )
        .await?;
        tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        claim
    };

    // (4)+(5): drive the claimed/duplicate refund to `issued` (provider call OUTSIDE any txn).
    match claim {
        ClaimResult::Claimed(refund_id) | ClaimResult::DuplicateSameBody(refund_id) => {
            drive_pending_refund(conn, provider, &refund_id).await
        }
        // No over-collection under the lock → nothing to refund (a concurrent refund already
        // returned it). Surface as a Duplicate(no-op) so the bridge reports true_up = 0.
        ClaimResult::NoOp => Ok(RefundOutcome::Duplicate(String::new())),
        ClaimResult::OverRefund(msg) => Ok(RefundOutcome::OverRefund(msg)),
        ClaimResult::Conflict => Ok(RefundOutcome::Conflict),
    }
}

/// Recompute the over-collection and claim the true-up `refunds` row, SERIALIZED per creator
/// (H1). Like [`claim_refund_locked`] but the AMOUNT is derived UNDER the lock from the cash
/// anchor, so it can never be stale:
///
/// ```text
/// over = cash_collected(invoice) − cash_refunds_already(invoice) − reissued_total   (floored 0)
/// ```
///
/// `over ≤ 0` ⇒ [`ClaimResult::NoOp`] (a concurrent refund already returned the
/// over-collection; nothing to do). Otherwise the true-up is tax-degenerate at the USD launch
/// (subtotal = amount, tax = 0) and claimed exactly as a cash refund. The cap holds by
/// construction: `cash_refunds_already + over = cash_collected − reissued_total ≤ cash_collected`.
async fn claim_true_up_locked<C: GenericClient + Sync>(
    tx: &C,
    creator_id: &uuid::Uuid,
    invoice_id: &str,
    reissued_total: i64,
    currency: &str,
    reason: Option<&str>,
    idempotency_key: &str,
) -> Result<ClaimResult, RegistryError> {
    // SERIALIZE per creator — the first act of the txn (mirrors claim_refund_locked).
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtext($1::text)::bigint)",
        &[&creator_id.to_string()],
    )
    .await
    .map_err(|e| RegistryError::Database(e.to_string()))?;

    // Recompute the over-collection UNDER the lock so it reflects every committed sibling
    // refund/dispute — the H1 fix. cash_collected already nets dispute_debit/_reversal rows.
    let cash = cash_collected(tx, invoice_id).await?;
    let cash_refunds_already =
        refunds_total_for_destination(tx, invoice_id, RefundDestination::Cash).await?;
    let over = (cash - cash_refunds_already - reissued_total).max(0);
    if over <= 0 {
        return Ok(ClaimResult::NoOp);
    }

    // Tax-degenerate true-up: subtotal = amount, tax = 0. Claim it as a cash refund through
    // the SAME locked precheck + claim as the operator path, so the over-refund trigger backs
    // it up identically.
    claim_refund_locked(
        tx, creator_id, invoice_id, over, over, 0, currency, RefundDestination::Cash, reason,
        idempotency_key,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn issue_refund_inner<C: GenericClient + Sync, P: RefundProvider>(
    conn: &mut C,
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
    // refundable. Read its currency + creator: the creator_id keys the advisory lock
    // the claim txn takes (CRITICAL-1).
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
    let creator_id: uuid::Uuid = row.get("creator_id");
    let currency: String = row.get("currency");
    let status: String = row.get("status");
    // A finalized invoice is refundable. The true-up bridge (`allow_voided`) also
    // refunds a deliberately-VOIDED invoice's over-collection — the cap reads
    // Σ(invoice_payments), which survives the void, so it stays money-correct. A draft
    // invoice is never refundable.
    let refundable = status == "finalized" || (allow_voided && status == "void");
    if !refundable {
        return Ok(RefundOutcome::InvalidInvoice(format!(
            "invoice {invoice_id} is {status} — not refundable"
        )));
    }

    // (2)+(3) CRITICAL-1: precheck + claim under the per-creator advisory lock so the
    // over-refund bound holds under concurrency. The claim runs in its OWN txn (a
    // trigger RAISE rolls back only the claim, never the caller's connection state) and
    // commits BEFORE the provider call — a network call must never hold the lock open.
    let claim = {
        let tx = conn.transaction().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        let claim = claim_refund_locked(
            &tx, &creator_id, invoice_id, amount_cents, subtotal_cents, tax_cents, &currency,
            destination, reason, idempotency_key,
        )
        .await?;
        // OverRefund / Conflict need no committed row; but committing is harmless (the
        // claim INSERT either did nothing or wrote a `pending` row we now drive).
        tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
        claim
    };

    // (4)+(5): drive the claimed/duplicate refund to `issued` (provider call OUTSIDE
    // any txn). Re-driving a refund WITH a ref is a no-op skip.
    match claim {
        ClaimResult::Claimed(refund_id) | ClaimResult::DuplicateSameBody(refund_id) => {
            drive_pending_refund(conn, provider, &refund_id).await
        }
        ClaimResult::OverRefund(msg) => Ok(RefundOutcome::OverRefund(msg)),
        ClaimResult::Conflict => Ok(RefundOutcome::Conflict),
        // The operator refund path passes a fixed amount > 0 to claim_refund_locked, which
        // never yields NoOp — only the true-up's amount-under-lock recompute does.
        ClaimResult::NoOp => Ok(RefundOutcome::Duplicate(String::new())),
    }
}

/// The outcome of [`claim_refund_locked`]: the locked precheck + claim step.
/// `pub` so the lock regression test can drive the claim on a caller-held tx and
/// observe the per-creator advisory lock (mirroring the PR-2 consume-lock test); not
/// part of the operator surface.
#[derive(Debug)]
pub enum ClaimResult {
    /// A fresh `pending` row was claimed; carries its `ref_…` id (drive it next).
    Claimed(String),
    /// The idempotency key was reused with the SAME body — the first refund's id
    /// (re-drive it to converge a prior crash; a fully-issued one is a no-op skip).
    DuplicateSameBody(String),
    /// Over the cash-anchored cap (precheck or the trigger backstop). No row claimed.
    OverRefund(String),
    /// Idempotency key reused with a DIFFERENT body. No row claimed.
    Conflict,
    /// Nothing to claim: the recomputed amount was `≤ 0` (only the true-up path produces
    /// this — the over-collection vanished under the lock). No row claimed; not an error.
    NoOp,
}

/// Precheck the over-refund bound and claim the `refunds` row, SERIALIZED per creator.
///
/// MUST run inside a transaction (`tx`): the FIRST statement is
/// `pg_advisory_xact_lock(hashtext(creator_id::text)::bigint)` — the SAME key
/// [`crate::credit::consume_at_finalize`] takes — so two concurrent refunds for one
/// creator serialize and the second sees the first's committed `pending` row. The
/// over-refund trigger (`0049`) is the single-statement BACKSTOP; this lock is what
/// makes the cap hold under concurrency.
///
/// `pub` only so the lock regression test can drive it directly on a caller-held tx (as
/// the PR-2 consume-lock test drives `consume_at_finalize`); the operator path goes
/// through [`issue_refund`].
#[allow(clippy::too_many_arguments)]
pub async fn claim_refund_locked<C: GenericClient + Sync>(
    tx: &C,
    creator_id: &uuid::Uuid,
    invoice_id: &str,
    amount_cents: i64,
    subtotal_cents: i64,
    tax_cents: i64,
    currency: &str,
    destination: RefundDestination,
    reason: Option<&str>,
    idempotency_key: &str,
) -> Result<ClaimResult, RegistryError> {
    // SERIALIZE per creator — the first act of the txn (mirrors consume_at_finalize).
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtext($1::text)::bigint)",
        &[&creator_id.to_string()],
    )
    .await
    .map_err(|e| RegistryError::Database(e.to_string()))?;

    let fingerprint =
        refund_fingerprint(invoice_id, amount_cents, subtotal_cents, tax_cents, destination);

    // Rust-side over-refund precheck against Σ(invoice_payments). Under the lock this
    // sees every COMMITTED sibling refund; the DB trigger is the authoritative backstop.
    let cash = cash_collected(tx, invoice_id).await?;
    let prior_cash = refunds_total_for_destination(tx, invoice_id, RefundDestination::Cash).await?;
    let prior_credit =
        refunds_total_for_destination(tx, invoice_id, RefundDestination::Credit).await?;
    let (new_cash, new_credit) = match destination {
        RefundDestination::Cash => (prior_cash + amount_cents, prior_credit),
        RefundDestination::Credit => (prior_cash, prior_credit + amount_cents),
    };
    if new_cash > cash || new_credit > cash || new_cash + new_credit > cash {
        return Ok(ClaimResult::OverRefund(format!(
            "refund of {amount_cents} ({}) would exceed cash collected {cash} on invoice {invoice_id} \
             (existing cash {prior_cash} / credit {prior_credit})",
            destination.as_str()
        )));
    }

    // Claim. INSERT … ON CONFLICT (idempotency_key) DO NOTHING. The over-refund trigger
    // fires on this INSERT; if it RAISEs we surface OverRefund.
    let refund_id = zeroship_core::typed_id::new_refund_id();
    let claim = tx
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
                return Ok(ClaimResult::OverRefund(format!(
                    "over-refund trigger rejected the claim on invoice {invoice_id}"
                )));
            }
            return Err(RegistryError::Database(e.to_string()));
        }
    };

    if claim.first().is_none() {
        // Key hit — compare the stored fingerprint (safe retry vs reuse-conflict).
        let existing = tx
            .query(
                "SELECT id, request_fingerprint FROM zeroship.refunds WHERE idempotency_key = $1",
                &[&idempotency_key],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        let Some(er) = existing.first() else {
            return Ok(ClaimResult::Conflict);
        };
        let existing_id: String = er.get("id");
        let existing_fp: String = er.get("request_fingerprint");
        if existing_fp == fingerprint {
            return Ok(ClaimResult::DuplicateSameBody(existing_id));
        }
        return Ok(ClaimResult::Conflict);
    }

    Ok(ClaimResult::Claimed(refund_id))
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
            // grant's `note` carries the refund identity (one grant per refund). MAJOR-1:
            // the DURABLE dedup is the `credit_ledger_refund_to_credit_note_idx` partial
            // UNIQUE on that note — so a concurrent / crash-window re-drive can NEVER
            // double-grant, even without the lock. The SELECT below is a cheap fast-path
            // skip (avoids minting an id), and the INSERT is `ON CONFLICT DO NOTHING` so a
            // racing sibling that wins the unique simply no-ops here.
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
                     VALUES ($1, $2, 'refund_to_credit', $3, $4, $5, $6) \
                     ON CONFLICT (note) WHERE kind = 'refund_to_credit' DO NOTHING",
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
/// refund). `credit_ledger` carries no refund FK, so this note IS the refund identity;
/// the `credit_ledger_refund_to_credit_note_idx` partial UNIQUE over it (0048, MAJOR-1)
/// makes a duplicate `refund_to_credit` grant a DB impossibility.
#[must_use]
pub fn refund_to_credit_note(refund_id: &str) -> String {
    format!("refund_to_credit:{refund_id}")
}

/// Re-export so callers don't need to thread [`credit`] for the credit-back path.
pub use credit::CREDIT_CURRENCY;
