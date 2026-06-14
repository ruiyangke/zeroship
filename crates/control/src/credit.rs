//! Credit ledger — the append-only customer-balance primitive (billing-ops gap
//! #26, PR-2; design `0048 credit_ledger` + flow A "credit-apply at finalize").
//!
//! The Stripe customer-balance model: a creator's credit BALANCE is
//! `SUM(amount_cents)` over [`credit_ledger`], never a stored column (so it can
//! never drift from its history). A grant is a POSITIVE entry; consumption is one
//! NEGATIVE `consumed` entry PER DRAWN GRANT (`consumed_from_grant_id`), written at
//! invoice finalize — so credit-expiry attribution stays exact.
//!
//! This module owns the two operations PR-2 needs:
//!
//!   * [`grant`] — the operator `POST /billing/credit` write: an idempotency-keyed,
//!     body-fingerprinted append of a positive grant entry. A reused key with the
//!     SAME body returns the first grant (safe retry); a reused key with a DIFFERENT
//!     body is a 409 conflict (mirroring Stripe), never a silent second grant.
//!
//!   * [`consume_at_finalize`] — the reconciler's credit-apply step: draw the
//!     creator's consumable, non-expired, same-currency credit OLDEST-FIRST against
//!     the invoice subtotal, appending one `consumed` entry per drawn grant keyed to
//!     the draft invoice id. **Re-run idempotent:** if `consumed` entries already
//!     exist for this invoice id (a crash-window re-drive of the SAME claim), the
//!     applied credit is recomputed from those existing entries and NO new draw
//!     happens — so a reconcile re-run never double-consumes.
//!
//! Both run inside the reconciler's per-creator advisory-locked `conn.transaction()`
//! / on a bare connection, so they are generic over [`compio_postgres::GenericClient`].

use compio_postgres::GenericClient;
use sha2::{Digest, Sha256};

use crate::registry::RegistryError;

/// v1 is USD-pinned: every credit grant and every invoice is USD. The grant
/// boundary rejects any other currency; the consume query filters
/// `AND currency = $invoice_currency` so a stray non-USD grant is never drawn.
pub const CREDIT_CURRENCY: &str = "usd";

/// One consumed draw written at finalize: the `consumed` entry's id, the grant it
/// drew from, and the (positive) magnitude drawn from that grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumedDraw {
    pub entry_id: String,
    pub grant_id: String,
    pub amount_cents: i64,
}

/// The outcome of [`consume_at_finalize`]: the total credit applied to the
/// invoice subtotal (written into `invoices.credit_cents` by the finalize UPDATE)
/// and the per-grant draws appended (empty on a zero-credit invoice). On a re-run
/// of an already-consumed invoice, `applied_cents` is recomputed from the existing
/// `consumed` rows and `draws` is empty (nothing new was appended).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CreditApplied {
    pub applied_cents: i64,
    pub draws: Vec<ConsumedDraw>,
}

/// The SHA-256 over the canonical grant request — the body fingerprint a reused
/// idempotency key is compared against. A reused key with a DIFFERENT body
/// (amount / currency / kind / expiry) is a 409, never a silent second grant.
#[must_use]
pub fn grant_fingerprint(
    creator_id: &uuid::Uuid,
    amount_cents: i64,
    currency: &str,
    kind: &str,
    expires_at: Option<i64>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(creator_id.as_bytes());
    hasher.update(b"|");
    hasher.update(amount_cents.to_le_bytes());
    hasher.update(b"|");
    hasher.update(currency.as_bytes());
    hasher.update(b"|");
    hasher.update(kind.as_bytes());
    hasher.update(b"|");
    hasher.update(expires_at.map_or(-1i64, |e| e).to_le_bytes());
    hex::encode(hasher.finalize())
}

/// The result of an operator grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantOutcome {
    /// A fresh grant was appended; carries its `crd_…` id.
    Created(String),
    /// The idempotency key was reused with the SAME body — the first grant's id is
    /// returned (safe retry, no second grant).
    Duplicate(String),
    /// The idempotency key was reused with a DIFFERENT body — rejected (409). No
    /// grant was appended.
    Conflict,
}

/// Append an operator credit grant for `creator_id`, idempotency-keyed and
/// body-fingerprinted. `kind` is a positive grant kind (`grant`/`promo`/`goodwill`);
/// `consumed`/`void_reversal` are reconciler-internal and rejected here. `currency`
/// MUST be USD (v1 pin). A reused `idempotency_key` returns the first grant if the
/// body matches, else 409.
///
/// Generic over the client so it composes inside a transaction.
pub async fn grant<C: GenericClient + Sync>(
    conn: &C,
    creator_id: &uuid::Uuid,
    amount_cents: i64,
    currency: &str,
    kind: &str,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    note: Option<&str>,
    idempotency_key: &str,
) -> Result<GrantOutcome, RegistryError> {
    // Boundary validation: positive amount, positive grant kind, USD only.
    if amount_cents <= 0 {
        return Err(RegistryError::InvalidInput(format!(
            "credit grant amount must be > 0 (got {amount_cents})"
        )));
    }
    if !matches!(kind, "grant" | "promo" | "goodwill") {
        return Err(RegistryError::InvalidInput(format!(
            "credit grant kind must be one of grant/promo/goodwill (got {kind:?}); \
             consumed/void_reversal/refund_to_credit are not operator-grantable"
        )));
    }
    if !currency.eq_ignore_ascii_case(CREDIT_CURRENCY) {
        return Err(RegistryError::InvalidInput(format!(
            "credit grant currency must be {CREDIT_CURRENCY} (v1 USD-pinned); got {currency:?}"
        )));
    }
    if idempotency_key.is_empty() {
        return Err(RegistryError::InvalidInput(
            "credit grant requires a non-empty Idempotency-Key".to_string(),
        ));
    }
    let currency = currency.to_ascii_lowercase();
    let expires_unix = expires_at.map(|d| d.timestamp());
    let fingerprint = grant_fingerprint(creator_id, amount_cents, &currency, kind, expires_unix);

    let id = zeroship_core::typed_id::new_credit_id();
    // Claim: INSERT … ON CONFLICT (idempotency_key) DO NOTHING. The first writer
    // wins; a retry no-ops and we reconcile against the stored fingerprint below.
    let inserted = conn
        .query(
            "INSERT INTO zeroship.credit_ledger \
               (id, creator_id, kind, amount_cents, currency, expires_at, note, \
                idempotency_key, request_fingerprint) \
             VALUES ($1, $2, $3::text, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (idempotency_key) WHERE idempotency_key IS NOT NULL \
             DO NOTHING \
             RETURNING id",
            &[
                &id,
                creator_id,
                &kind,
                &amount_cents,
                &currency,
                &expires_at,
                &note,
                &idempotency_key,
                &fingerprint,
            ],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    if let Some(row) = inserted.first() {
        return Ok(GrantOutcome::Created(row.get::<_, String>("id")));
    }

    // Conflict: a grant with this idempotency_key already exists. Compare the
    // stored fingerprint to decide safe-retry (same body) vs reuse-conflict.
    let existing = conn
        .query(
            "SELECT id, request_fingerprint FROM zeroship.credit_ledger \
             WHERE idempotency_key = $1",
            &[&idempotency_key],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let Some(row) = existing.first() else {
        return Err(RegistryError::Database(
            "credit grant ON CONFLICT but no existing row found".to_string(),
        ));
    };
    let existing_id: String = row.get("id");
    let existing_fp: Option<String> = row.get("request_fingerprint");
    if existing_fp.as_deref() == Some(fingerprint.as_str()) {
        Ok(GrantOutcome::Duplicate(existing_id))
    } else {
        Ok(GrantOutcome::Conflict)
    }
}

/// One consumable grant: its id and the remaining (undrawn, positive) balance on it.
struct AvailableGrant {
    id: String,
    remaining: i64,
}

/// Consume the creator's available credit against `subtotal_cents` at finalize,
/// keyed to `invoice_id` (the draft invoice's id — stable across re-runs of the
/// SAME `(creator, period)` claim).
///
/// **Re-run idempotency (critical):** if `consumed` entries already reference this
/// `invoice_id`, the credit was applied on a prior pass of this SAME claim; we
/// recompute `applied_cents` from those existing rows and append NOTHING — so a
/// reconcile re-run (crash-window re-drive) never double-consumes. The
/// `status='finalized'` short-circuit in `bill_creator` covers the already-finalized
/// case; this guard covers the draft-re-drive case.
///
/// Otherwise: read consumable, non-expired, same-currency grants OLDEST-FIRST,
/// draw `min(Σ remaining, subtotal_cents)` from them in turn, and append one
/// `consumed` entry per drawn grant. Must run inside the caller's transaction so the
/// draws and the finalize UPDATE commit together.
pub async fn consume_at_finalize<C: GenericClient + Sync>(
    conn: &C,
    creator_id: &uuid::Uuid,
    invoice_id: &str,
    subtotal_cents: i64,
    invoice_currency: &str,
) -> Result<CreditApplied, RegistryError> {
    // RE-RUN GUARD: already-consumed against THIS invoice id? Recompute and bail.
    // `consumed` rows are negative; the applied total is the magnitude of their sum.
    let already = conn
        .query(
            "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS drawn \
             FROM zeroship.credit_ledger \
             WHERE applied_invoice_id = $1 AND kind = 'consumed'",
            &[&invoice_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let already_drawn: i64 = already.first().map_or(0, |r| r.get::<_, i64>("drawn"));
    if already_drawn < 0 {
        // Consumption already anchored to this invoice claim on a prior pass —
        // recompute the applied credit and append nothing (no double-consume).
        return Ok(CreditApplied {
            applied_cents: -already_drawn,
            draws: Vec::new(),
        });
    }

    if subtotal_cents <= 0 {
        return Ok(CreditApplied::default());
    }
    let currency = invoice_currency.to_ascii_lowercase();

    // Consumable grants OLDEST-FIRST: a positive (grant-class) entry, same currency,
    // not expired, with undrawn balance remaining. `remaining` = the grant amount
    // plus the (negative) sum of every prior `consumed` entry that drew from it.
    let grant_rows = conn
        .query(
            "SELECT g.id, \
                    (g.amount_cents + COALESCE(d.drawn, 0))::bigint AS remaining \
             FROM zeroship.credit_ledger g \
             LEFT JOIN ( \
                 SELECT consumed_from_grant_id, SUM(amount_cents) AS drawn \
                 FROM zeroship.credit_ledger \
                 WHERE kind = 'consumed' \
                 GROUP BY consumed_from_grant_id \
             ) d ON d.consumed_from_grant_id = g.id \
             WHERE g.creator_id = $1 \
               AND g.kind <> 'consumed' \
               AND g.amount_cents > 0 \
               AND g.currency = $2 \
               AND (g.expires_at IS NULL OR g.expires_at > NOW()) \
             ORDER BY g.created_at, g.id",
            &[creator_id, &currency],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let grants: Vec<AvailableGrant> = grant_rows
        .iter()
        .map(|r| AvailableGrant {
            id: r.get::<_, String>("id"),
            remaining: r.get::<_, i64>("remaining"),
        })
        .filter(|g| g.remaining > 0)
        .collect();

    let available: i64 = grants.iter().map(|g| g.remaining).sum();
    let applied = available.min(subtotal_cents);
    if applied <= 0 {
        return Ok(CreditApplied::default());
    }

    // Per-grant FIFO drawdown: draw `applied` from the oldest grants in turn,
    // appending one `consumed` entry per drawn grant.
    let mut remaining_to_draw = applied;
    let mut draws: Vec<ConsumedDraw> = Vec::new();
    for g in &grants {
        if remaining_to_draw <= 0 {
            break;
        }
        let draw = g.remaining.min(remaining_to_draw);
        if draw <= 0 {
            continue;
        }
        let entry_id = zeroship_core::typed_id::new_credit_id();
        conn.execute(
            "INSERT INTO zeroship.credit_ledger \
               (id, creator_id, kind, amount_cents, currency, applied_invoice_id, \
                consumed_from_grant_id) \
             VALUES ($1, $2, 'consumed', $3, $4, $5, $6)",
            &[&entry_id, creator_id, &(-draw), &currency, &invoice_id, &g.id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
        draws.push(ConsumedDraw {
            entry_id,
            grant_id: g.id.clone(),
            amount_cents: draw,
        });
        remaining_to_draw -= draw;
    }

    Ok(CreditApplied {
        applied_cents: applied,
        draws,
    })
}

/// A creator's current credit balance: `SUM(amount_cents)` over their ledger,
/// filtered by currency. Positive grants minus negative `consumed` entries. Used
/// by tests and the read API (PR-7). `0` when there are no entries.
pub async fn balance<C: GenericClient + Sync>(
    conn: &C,
    creator_id: &uuid::Uuid,
    currency: &str,
) -> Result<i64, RegistryError> {
    let currency = currency.to_ascii_lowercase();
    let rows = conn
        .query(
            "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS bal \
             FROM zeroship.credit_ledger WHERE creator_id = $1 AND currency = $2",
            &[creator_id, &currency],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map_or(0, |r| r.get::<_, i64>("bal")))
}
