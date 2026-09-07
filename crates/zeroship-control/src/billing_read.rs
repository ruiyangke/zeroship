//! Organization-facing billing READ surface (billing-ops gap #26, PR-7).
//!
//! The data layer behind the six `BillingRead` endpoints in [`crate::api`]:
//! invoice history, frozen-snapshot line detail, the current-period projected
//! charge, credit balance, payment-method status, and plan/spend-state. Every
//! HTTP handler is organization-scoped to OWNED apps (the `Resource::App{id}`
//! membership gate `get_spend_limit` uses) with operators reading any via
//! `Resource::Any`; this module holds the SQL + DTO assembly + the
//! projected-charge budget cache, keeping the handlers thin.
//!
//! SECURITY: nothing here leaks a raw Stripe id, another organization's data, or an
//! internal-only column — the DTOs are hand-shaped projections, never `SELECT *`.
//!
//! It also holds [`outstanding_billing`], the ONE answer to "what does this
//! organization still owe". That question is a billing read, it is asked from
//! three places that destroy something, and a second spelling of it would be a
//! second policy. See that function's header for the arms and for what each one
//! deliberately does not count.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::NaiveDate;
use compio_postgres::GenericClient;
use serde::Serialize;
use uuid::Uuid;

use crate::metering::{current_period_start_unix, period_date};
use crate::pricing::{charge_cents, PlanPrice};
use crate::pricing_store::PricingStore;
use crate::registry::{Registry, RegistryError};

// ---------------------------------------------------------------------------
// Projected-charge budget cache (MAJOR-5)
// ---------------------------------------------------------------------------

/// Short TTL (seconds) the open-period projection is cached for. The design
/// (read API G, MAJOR-5) calls for a 30–60s window; we use 60s. Usage only
/// GROWS within a period, so a slightly-stale projection is always a safe
/// LOWER bound for the next ≤60s — and the value is non-authoritative anyway
/// (only a finalized invoice bills). The cache is invalidated naturally by TTL.
pub const PROJECTED_CHARGE_TTL_SECS: i64 = 60;

/// Default cap on distinct `(app_id, period)` projection entries held in memory.
/// Sized for the fleet's hot-app working set; an over-cap insert evicts an
/// arbitrary entry (the value is cheap to recompute on the next miss).
pub const PROJECTED_CHARGE_CACHE_MAX_ENTRIES: usize = 50_000;

/// One cached projection: the computed cents + the wall-clock second it expires.
#[derive(Debug, Clone, Copy)]
struct CachedProjection {
    projected_charge_cents: u64,
    /// The `as_of` instant the value was computed (echoed to the client).
    as_of_unix: i64,
    expires_at_unix: i64,
}

/// In-process TTL cache for the open-period projected charge, keyed
/// `(app_id, period)` — the budget backstop for read API G (MAJOR-5).
///
/// Re-pricing on EVERY poll is an unbudgeted compute amplifier (an organization or a
/// script can hammer a full `charge_cents` pass over live aggregates). This
/// cache bounds that: within [`PROJECTED_CHARGE_TTL_SECS`] a second call for the
/// same `(app, period)` returns the memoised value WITHOUT touching Postgres or
/// re-running pricing. The peer of [`zeroship_core::logout_token::LogoutJtiCache`]
/// — a `std::Mutex<HashMap>` (zero-tokio, no async lock), self-pruning on read.
///
/// `reprice_count` is a monotonically-increasing counter of how many times a
/// projection was actually COMPUTED (a cache MISS). Tests assert that a second
/// call within the TTL does NOT advance it — i.e. the budget actually holds.
#[derive(Debug)]
pub struct ProjectedChargeCache {
    inner: Mutex<HashMap<(Uuid, NaiveDate), CachedProjection>>,
    max_entries: usize,
    /// Total number of cache MISSES served by an actual `charge_cents` pass.
    /// Observability + the faithful test assertion for "the cache works".
    reprice_count: std::sync::atomic::AtomicU64,
}

impl ProjectedChargeCache {
    #[must_use]
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_entries,
            reprice_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Look up a still-live cached projection for `(app_id, period)`. Returns
    /// `None` on a miss or an expired entry (which is pruned in passing).
    ///
    /// # Panics
    /// Panics if the internal `Mutex` is poisoned.
    fn get(&self, app_id: &Uuid, period: NaiveDate, now_unix: i64) -> Option<CachedProjection> {
        let mut guard = self.inner.lock().expect("poisoned");
        match guard.get(&(*app_id, period)).copied() {
            Some(hit) if hit.expires_at_unix > now_unix => Some(hit),
            Some(_) => {
                // Expired — drop it so the map does not grow unbounded with stale keys.
                guard.remove(&(*app_id, period));
                None
            }
            None => None,
        }
    }

    /// Insert a freshly-computed projection, stamping its expiry.
    ///
    /// # Panics
    /// Panics if the internal `Mutex` is poisoned.
    fn put(&self, app_id: Uuid, period: NaiveDate, value: u64, now_unix: i64) {
        let mut guard = self.inner.lock().expect("poisoned");
        // Bound memory: evict an arbitrary live entry when at capacity (the value
        // is cheap to recompute on the next miss).
        if guard.len() >= self.max_entries && !guard.contains_key(&(app_id, period)) {
            if let Some(victim) = guard.keys().next().copied() {
                guard.remove(&victim);
            }
        }
        guard.insert(
            (app_id, period),
            CachedProjection {
                projected_charge_cents: value,
                as_of_unix: now_unix,
                expires_at_unix: now_unix + PROJECTED_CHARGE_TTL_SECS,
            },
        );
    }

    /// How many times a projection was actually COMPUTED (cache MISSES). Used by
    /// the faithful PR-7 test to prove a within-TTL second call did NOT re-price.
    #[must_use]
    pub fn reprice_count(&self) -> u64 {
        self.reprice_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for ProjectedChargeCache {
    fn default() -> Self {
        Self::new(PROJECTED_CHARGE_CACHE_MAX_ENTRIES)
    }
}

/// The non-authoritative projected-charge response (read API G).
#[derive(Debug, Clone, Serialize)]
pub struct ProjectedCharge {
    /// The re-priced current-period charge over LIVE aggregates, in cents.
    pub projected_charge_cents: u64,
    /// ALWAYS `false`. A projection is never a bill — only a finalized invoice
    /// is authoritative. A mid-period weight/FX change or proration means the
    /// finalized total may differ. The label is load-bearing, not decoration.
    pub authoritative: bool,
    /// The first-of-month period this projects (ISO date).
    pub period: String,
    /// Unix seconds the projection was computed (the cached value's age anchor).
    pub as_of: i64,
}

/// Compute (or serve from cache) the current-period projected charge for one
/// app, re-pricing the OPEN period's LIVE `usage_aggregates` under the app's
/// CURRENT plan via [`charge_cents`] — the same pricing kernel the reconciler
/// runs at finalize.
///
/// **Non-authoritative by construction** (MAJOR-5): this prices the still-
/// growing period as a SINGLE full-period segment under `apps.plan_id`. It does
/// NOT replay intra-period plan-change segments / proration (the reconciler
/// does that at finalize), so the finalized bill may differ. The response is
/// labelled `authoritative: false`.
///
/// **Budgeted** (MAJOR-5): a within-TTL repeat for the same `(app, period)` is
/// a cache HIT — no DB read, no pricing pass (`reprice_count` does not advance).
///
/// `Ok(None)` when the app row does not exist.
///
/// # Errors
/// - [`RegistryError`] on a DB failure.
/// - A pricing failure (unresolved FX / CU overflow) maps to
///   [`RegistryError::FxUnresolved`] so the handler can surface a clean 5xx rather
///   than fabricating a $0 projection.
pub async fn projected_charge(
    registry: &Registry,
    cache: &ProjectedChargeCache,
    app_id: &Uuid,
    now_unix: i64,
) -> Result<Option<ProjectedCharge>, RegistryError> {
    let period_start = current_period_start_unix();
    let period = period_date(period_start);

    // Cache HIT: serve the memoised value, no DB, no re-price.
    if let Some(hit) = cache.get(app_id, period, now_unix) {
        return Ok(Some(ProjectedCharge {
            projected_charge_cents: hit.projected_charge_cents,
            authoritative: false,
            period: period.to_string(),
            as_of: hit.as_of_unix,
        }));
    }

    // MISS: confirm the app exists + resolve its current plan id.
    let conn = registry.conn().await?;
    let app_rows = conn
        .query("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[app_id])
        .await?;
    let Some(app_row) = app_rows.first() else {
        return Ok(None);
    };
    let plan_id: String = app_row.get("plan_id");

    // Live, still-growing usage for the OPEN period (no closed-period claim).
    let usage_rows = conn
        .query(
            "SELECT metric, total FROM zeroship.usage_aggregates \
             WHERE app_id = $1 AND period = $2::date",
            &[app_id, &period],
        )
        .await?;
    let mut usage: HashMap<String, i64> = HashMap::with_capacity(usage_rows.len());
    for row in &usage_rows {
        usage.insert(row.get("metric"), row.get("total"));
    }

    // Resolve the (FX-effective) plan price + the global weight table — the SAME
    // inputs the reconciler's `charge_cents` consumes.
    let pricing = PricingStore::new(registry.clone());
    let weights = pricing.weights().await?;
    let default_fx = pricing.default_fx_pico_cents_per_unit().await?;
    let catalog = crate::plan_catalog::PlanCatalog::new(registry.clone());
    let price: PlanPrice = match catalog.get(&plan_id).await? {
        Some(plan) => plan.price.with_effective_fx(default_fx),
        // The plan vanished from the catalog (archived/deleted). Price base-only
        // is wrong; surface as a pricing error so the handler 5xxs rather than
        // projecting a misleading $0.
        None => return Err(RegistryError::FxUnresolved),
    };

    // Re-price. A pricing failure (unresolved FX / overflow) is a loud error,
    // never a silent $0 (mirrors the reconciler's fail-closed posture).
    let breakdown = charge_cents(&price, &usage, &weights).map_err(|e| {
        tracing::warn!(app_id = %app_id, error = %e, "billing_read: projected-charge pricing failed");
        RegistryError::FxUnresolved
    })?;

    // Count the COMPUTE (miss) and memoise under the TTL.
    cache
        .reprice_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    cache.put(*app_id, period, breakdown.total_cents, now_unix);

    Ok(Some(ProjectedCharge {
        projected_charge_cents: breakdown.total_cents,
        authoritative: false,
        period: period.to_string(),
        as_of: now_unix,
    }))
}

// ---------------------------------------------------------------------------
// Invoice history + frozen-snapshot line detail
// ---------------------------------------------------------------------------

/// One row of an organization's invoice history (read API: invoice history).
#[derive(Debug, Clone, Serialize)]
pub struct InvoiceSummary {
    pub id: String,
    pub period: String,
    pub status: String,
    pub currency: String,
    pub subtotal_cents: i64,
    pub credit_cents: i64,
    pub tax_cents: i64,
    pub total_cents: i64,
    pub finalized_at: Option<String>,
}

/// List an organization's invoices newest-first, scoped to the apps they OWN through
/// the invoice's `organization_id`. `limit`/`offset` paginate (clamped by the
/// handler). The invoice is organization-keyed, so the scope is `organization_id = $1`.
///
/// # Errors
/// [`RegistryError`] on a DB failure.
pub async fn list_invoices_for_organization(
    registry: &Registry,
    organization_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<InvoiceSummary>, RegistryError> {
    let conn = registry.conn().await?;
    let rows = conn
        .query(
            "SELECT id, period::text AS period, status, currency, \
                    subtotal_cents, credit_cents, tax_cents, total_cents, \
                    finalized_at::text AS finalized_at \
             FROM zeroship.invoices \
             WHERE organization_id = $1 \
             ORDER BY period DESC, created_at DESC \
             LIMIT $2 OFFSET $3",
            &[&organization_id, &limit, &offset],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| InvoiceSummary {
            id: r.get("id"),
            period: r.get("period"),
            status: r.get("status"),
            currency: r.get("currency"),
            subtotal_cents: r.get("subtotal_cents"),
            credit_cents: r.get("credit_cents"),
            tax_cents: r.get("tax_cents"),
            total_cents: r.get("total_cents"),
            finalized_at: r.get("finalized_at"),
        })
        .collect())
}

/// One frozen per-app/per-segment invoice line (read API: line detail). This is
/// the reproducibility record: `usage_snapshot` + `weights_snapshot` + the
/// applied `included_units`/`fx` reproduce `amount_cents` bit-for-bit via
/// `charge_cents`.
#[derive(Debug, Clone, Serialize)]
pub struct InvoiceLineDetail {
    pub app_id: Uuid,
    pub segment_no: i16,
    pub plan_id: String,
    pub included_units: i64,
    pub fx_pico_cents_per_unit: i64,
    pub base_fee_cents: i64,
    pub amount_cents: i64,
    pub usage_snapshot: serde_json::Value,
    pub weights_snapshot: serde_json::Value,
    /// Gross compute units derived from the frozen `usage_snapshot` +
    /// `weights_snapshot` — the SAME CU shown on the Stripe invoice line, so the
    /// dashboard agrees with what the organization sees on Stripe. `None` only if the
    /// frozen snapshot is malformed (an impossible state for a posted line).
    pub compute_units: Option<u64>,
    /// `max(0, compute_units − included_units)` — the post-included-units billable
    /// CU. `None` mirrors `compute_units`.
    pub billable_units: Option<u64>,
}

/// Derive `(compute_units, billable_units)` from a frozen line's
/// `usage_snapshot`, `weights_snapshot`, and `included_units`, reusing the
/// EXACT pricing arithmetic (`pricing::total_units`). Returns `(None, None)`
/// if the snapshot JSON can't be parsed (an impossible state for a posted
/// line) so the read API degrades to the existing fields rather than
/// erroring.
fn derive_line_cu(
    usage_snapshot: &serde_json::Value,
    weights_snapshot: &serde_json::Value,
    included_units: i64,
) -> (Option<u64>, Option<u64>) {
    let Ok(usage) =
        serde_json::from_value::<std::collections::HashMap<String, i64>>(usage_snapshot.clone())
    else {
        return (None, None);
    };
    let Ok(weights) = serde_json::from_value::<crate::pricing::MetricWeights>(weights_snapshot.clone())
    else {
        return (None, None);
    };
    match crate::pricing::total_units(&weights, &usage) {
        Ok(total) => {
            let included = u64::try_from(included_units).unwrap_or(0);
            (Some(total), Some(total.saturating_sub(included)))
        }
        Err(_) => (None, None),
    }
}

/// The full line-detail view of one invoice: its money envelope + every frozen
/// segment line, ordered `(app_id, segment_no)`.
#[derive(Debug, Clone, Serialize)]
pub struct InvoiceDetail {
    #[serde(flatten)]
    pub summary: InvoiceSummary,
    pub organization_id: String,
    pub lines: Vec<InvoiceLineDetail>,
}

/// Load one invoice's money envelope + frozen lines, returning its `organization_id`
/// so the handler can authz-scope by app membership. `Ok(None)` when the
/// invoice does not exist.
///
/// # Errors
/// [`RegistryError`] on a DB failure.
pub async fn get_invoice_detail(
    registry: &Registry,
    invoice_id: &str,
) -> Result<Option<InvoiceDetail>, RegistryError> {
    let conn = registry.conn().await?;
    let inv_rows = conn
        .query(
            "SELECT id, organization_id, period::text AS period, status, currency, \
                    subtotal_cents, credit_cents, tax_cents, total_cents, \
                    finalized_at::text AS finalized_at \
             FROM zeroship.invoices WHERE id = $1",
            &[&invoice_id],
        )
        .await?;
    let Some(inv) = inv_rows.first() else {
        return Ok(None);
    };
    let organization_id: String = inv.get("organization_id");
    let summary = InvoiceSummary {
        id: inv.get("id"),
        period: inv.get("period"),
        status: inv.get("status"),
        currency: inv.get("currency"),
        subtotal_cents: inv.get("subtotal_cents"),
        credit_cents: inv.get("credit_cents"),
        tax_cents: inv.get("tax_cents"),
        total_cents: inv.get("total_cents"),
        finalized_at: inv.get("finalized_at"),
    };

    let line_rows = conn
        .query(
            "SELECT app_id, segment_no, plan_id, included_units, fx_pico_cents_per_unit, \
                    base_fee_cents, amount_cents, usage_snapshot, weights_snapshot \
             FROM zeroship.invoice_lines \
             WHERE invoice_id = $1 \
             ORDER BY app_id, segment_no",
            &[&invoice_id],
        )
        .await?;
    let lines = line_rows
        .iter()
        .map(|r| {
            let included_units: i64 = r.get("included_units");
            let usage_snapshot: serde_json::Value = r.get("usage_snapshot");
            let weights_snapshot: serde_json::Value = r.get("weights_snapshot");
            let (compute_units, billable_units) =
                derive_line_cu(&usage_snapshot, &weights_snapshot, included_units);
            InvoiceLineDetail {
                app_id: r.get("app_id"),
                segment_no: r.get("segment_no"),
                plan_id: r.get("plan_id"),
                included_units,
                fx_pico_cents_per_unit: r.get("fx_pico_cents_per_unit"),
                base_fee_cents: r.get("base_fee_cents"),
                amount_cents: r.get("amount_cents"),
                usage_snapshot,
                weights_snapshot,
                compute_units,
                billable_units,
            }
        })
        .collect();

    Ok(Some(InvoiceDetail {
        summary,
        organization_id,
        lines,
    }))
}

// ---------------------------------------------------------------------------
// Credit balance + recent ledger
// ---------------------------------------------------------------------------

/// One recent credit-ledger entry, surfaced for transparency (grant/consume).
/// Internal join columns (`consumed_from_grant_id`, fingerprints, idempotency
/// keys) are NOT exposed.
#[derive(Debug, Clone, Serialize)]
pub struct CreditLedgerEntry {
    pub id: String,
    pub kind: String,
    pub amount_cents: i64,
    pub currency: String,
    pub applied_invoice_id: Option<String>,
    pub note: Option<String>,
    pub expires_at: Option<String>,
    pub created_at: String,
}

/// An organization's credit balance (USD) + their most-recent ledger entries.
#[derive(Debug, Clone, Serialize)]
pub struct CreditBalance {
    /// `SUM(credit_ledger.amount_cents)` filtered to USD — the consumable
    /// balance. Non-negative by the consume invariant; surfaced as `i64`.
    pub balance_cents: i64,
    pub currency: String,
    pub recent: Vec<CreditLedgerEntry>,
}

/// Read an organization's USD credit balance + recent ledger entries. Reuses
/// [`crate::credit::balance`] for the authoritative SUM, then loads up to
/// `recent_limit` newest entries for transparency.
///
/// # Errors
/// [`RegistryError`] on a DB failure.
pub async fn credit_balance(
    registry: &Registry,
    organization_id: &str,
    recent_limit: i64,
) -> Result<CreditBalance, RegistryError> {
    let conn = registry.conn().await?;
    let balance_cents = crate::credit::balance(&conn, organization_id, "usd").await?;
    let rows = conn
        .query(
            "SELECT id, kind, amount_cents, currency, applied_invoice_id, note, \
                    expires_at::text AS expires_at, created_at::text AS created_at \
             FROM zeroship.credit_ledger \
             WHERE organization_id = $1 AND currency = 'usd' \
             ORDER BY created_at DESC, id DESC \
             LIMIT $2",
            &[&organization_id, &recent_limit],
        )
        .await?;
    let recent = rows
        .iter()
        .map(|r| CreditLedgerEntry {
            id: r.get("id"),
            kind: r.get("kind"),
            amount_cents: r.get("amount_cents"),
            currency: r.get("currency"),
            applied_invoice_id: r.get("applied_invoice_id"),
            note: r.get("note"),
            expires_at: r.get("expires_at"),
            created_at: r.get("created_at"),
        })
        .collect();
    Ok(CreditBalance {
        balance_cents,
        currency: "usd".to_string(),
        recent,
    })
}

// ---------------------------------------------------------------------------
// Payment-method status (no raw provider ids leaked)
// ---------------------------------------------------------------------------

/// Whether a default payment method is on file — STATUS ONLY. The raw
/// `cus_…`/`external_id` is NEVER surfaced (only its presence, as a bool).
#[derive(Debug, Clone, Serialize)]
pub struct PaymentMethodStatus {
    /// `organization_billing.default_pm_set` — has the organization attached a default PM.
    pub default_pm_set: bool,
    /// Whether a Stripe customer ref exists for this organization (presence only).
    pub customer_ref_present: bool,
}

/// Read an organization's payment-method STATUS: the `default_pm_set` flag plus
/// whether a `billing_customer_refs` row exists — never the raw provider id.
///
/// # Errors
/// [`RegistryError`] on a DB failure.
pub async fn payment_method_status(
    registry: &Registry,
    organization_id: &str,
) -> Result<PaymentMethodStatus, RegistryError> {
    let conn = registry.conn().await?;
    let rows = conn
        .query(
            "SELECT cb.default_pm_set, \
                    EXISTS ( \
                       SELECT 1 FROM zeroship.billing_customer_refs r \
                       WHERE r.organization_id = cb.organization_id AND r.provider = 'stripe' \
                    ) AS customer_ref_present \
             FROM zeroship.organization_billing cb WHERE cb.organization_id = $1",
            &[&organization_id],
        )
        .await?;
    // No organization_billing row yet ⇒ no PM, no ref. An organization who has never been
    // billed simply has nothing on file (not an error).
    let Some(row) = rows.first() else {
        return Ok(PaymentMethodStatus {
            default_pm_set: false,
            customer_ref_present: false,
        });
    };
    Ok(PaymentMethodStatus {
        default_pm_set: row.get("default_pm_set"),
        customer_ref_present: row.get("customer_ref_present"),
    })
}

// ---------------------------------------------------------------------------
// Plan + spend state
// ---------------------------------------------------------------------------

/// An app's plan + spend cap + spend/account state (read API: plan/spend-state).
#[derive(Debug, Clone, Serialize)]
pub struct BillingStatus {
    pub plan_id: String,
    /// The effective spend cap in cents (`override ?? plan_default`).
    pub effective_limit_cents: u64,
    pub override_cents: Option<i64>,
    pub plan_default_cents: u64,
    /// `app_spend_state.state` — one of `allow`/`warn`/`degrade`/`block`.
    pub spend_state: String,
    /// `organization_billing_status.state` — one of `active`/`past_due`/`suspended`.
    pub account_state: String,
}

/// Read an app's plan + effective spend limit + spend/account state. Mirrors
/// `get_spend_limit`'s resolution (`override ?? plan_default`) and joins the
/// owning organization's account state. `Ok(None)` when the app row is missing.
///
/// # Errors
/// [`RegistryError`] on a DB failure.
pub async fn billing_status(
    registry: &Registry,
    app_id: &Uuid,
) -> Result<Option<BillingStatus>, RegistryError> {
    let conn = registry.conn().await?;
    let rows = conn
        .query(
            // THE ACCOUNT-STATE JOIN IS BYTE-FOR-BYTE THE ONE
            // `registry::get_routes` USES: `organization_billing_status` keyed
            // on `apps.organization_id`. That identity is the point - this read
            // backs the console and that one backs the edge, so any difference
            // between them is a console that disagrees with what a request
            // actually gets.
            "SELECT a.plan_id, \
                    l.spend_limit_cents AS override_cents, \
                    COALESCE(s.state, 'allow')   AS spend_state, \
                    COALESCE(obs.state, 'active') AS account_state \
             FROM zeroship.apps a \
             LEFT JOIN zeroship.app_spend_limit l ON l.app_id = a.id \
             LEFT JOIN zeroship.app_spend_state s ON s.app_id = a.id \
             LEFT JOIN zeroship.organization_billing_status obs \
                    ON obs.organization_id = a.organization_id \
             WHERE a.id = $1",
            &[app_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let plan_id: String = row.get("plan_id");
    let override_cents: Option<i64> = row.get("override_cents");
    let spend_state: String = row.get("spend_state");
    let account_state: String = row.get("account_state");

    let catalog = crate::plan_catalog::PlanCatalog::new(registry.clone());
    let plan_default = catalog
        .get(&plan_id)
        .await?
        .map_or(0, |p| p.price.spend_limit_default_cents);
    let effective = override_cents
        .and_then(|o| u64::try_from(o).ok())
        .unwrap_or(plan_default);

    Ok(Some(BillingStatus {
        plan_id,
        effective_limit_cents: effective,
        override_cents,
        plan_default_cents: plan_default,
        spend_state,
        account_state,
    }))
}

// ---------------------------------------------------------------------------
// What an organization still owes — the ONE deletion predicate
// ---------------------------------------------------------------------------

/// Whether the configured invoicer writes into `zeroship.invoices`.
///
/// `lago` and `stripe_meters` implement `close_period` as an `InvoiceRef(None)`
/// and write NO local invoice row, so under them `zeroship.invoices` is empty by
/// construction. "Usage in a closed period with no invoice" then describes every
/// organization that ever served a request, and a refusal built on it would
/// block every deletion forever with nothing anyone could do. The
/// unbilled-usage arm is therefore asked only when the invoicer owns the local
/// rail (`BillingStack::invoicer_owns_local_invoice`, true for `lite` and
/// `stripe_invoice`).
///
/// The UNPAID-INVOICE arm is deliberately NOT conditioned on this. A finalized
/// invoice row that is short of cash is a debt whoever wrote it, and a
/// deployment moved from `lite` to `lago` leaves exactly those rows standing;
/// skipping them under the new stack would write them off silently. Under a
/// non-local invoicer the arm reads an empty table and costs one indexed scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalInvoicing {
    Yes,
    No,
}

impl LocalInvoicing {
    /// Derived from the running stack, in ONE place, so the three enforcement
    /// points cannot disagree about which provider they are asking about.
    #[must_use]
    pub fn of(stack: &crate::metering::provider::BillingStack) -> Self {
        if stack.invoicer_owns_local_invoice() {
            Self::Yes
        } else {
            Self::No
        }
    }

    const fn asks_about_unbilled_usage(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// One finalized invoice with cash still owing on it.
#[derive(Debug, Clone, Serialize)]
pub struct UnpaidInvoice {
    pub invoice_id: String,
    pub period: String,
    pub currency: String,
    /// Already NET of credit — the `invoice_total_balances` CHECK is
    /// `total = subtotal - credit + tax`, so a credit lowers this number rather
    /// than sitting beside it. There is no "nonzero total that is not owed".
    pub total_cents: i64,
    /// `Sum(invoice_payments.amount_cents)` — the same oracle
    /// [`crate::invoice_payments::cash_collected`] reads, and for the same
    /// reason: a payment is an append-only side fact because
    /// `invoices_immutable()` makes a paid COLUMN on the frozen invoice
    /// mechanically impossible. Dispute rows are signed, so a chargeback
    /// lowers this and re-opens the debt.
    pub cash_collected_cents: i64,
    /// `total_cents - cash_collected_cents`, positive on every row that is here.
    pub owed_cents: i64,
}

/// One CLOSED billing period whose usage never became an invoice.
#[derive(Debug, Clone, Serialize)]
pub struct UnbilledPeriod {
    pub period: String,
    pub apps: i64,
    /// Raw metered units, NOT money. Turning units into cents means re-running
    /// the pricing kernel over live aggregates, which is what
    /// [`projected_charge`] does and labels `authoritative: false`; a refusal
    /// must not quote a figure that is not a bill.
    pub units: i64,
}

/// The one action that clears a billing blocker. Both arms name a route that
/// exists today: attaching a default payment method is the ONLY remedy wired
/// end to end (`credit::grant`, `refund::issue_refund` and
/// `void_reissue::void_and_reissue` are complete and reachable only from tests),
/// and it is also what lets the reconciler bill usage it is currently skipping
/// for want of a Stripe Customer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingRemedy {
    /// A finalized invoice is short of cash.
    SettleInvoices,
    /// Closed-period usage never became an invoice.
    BillOutstandingUsage,
}

impl BillingRemedy {
    /// One sentence a person can act on, naming the route.
    #[must_use]
    pub const fn instruction(self) -> &'static str {
        match self {
            Self::SettleInvoices => {
                "attach a default payment method with \
                 POST /api/organizations/{organization_id}/billing/setup; Stripe then \
                 collects the open invoice"
            }
            Self::BillOutstandingUsage => {
                "this organization has usage in a closed period that was never invoiced, \
                 which is what happens when no Stripe Customer exists to bill; attach a \
                 default payment method with \
                 POST /api/organizations/{organization_id}/billing/setup so the next \
                 billing sweep can bill and collect it"
            }
        }
    }
}

/// Everything one organization still owes, as facts rather than a bool.
///
/// A bool cannot carry a refusal message: the person on the other end has to be
/// told WHAT is owed and WHICH action clears it, and both are derived from the
/// rows below rather than stored anywhere.
#[derive(Debug, Clone, Serialize)]
pub struct OutstandingBilling {
    pub organization_id: String,
    pub unpaid_invoices: Vec<UnpaidInvoice>,
    pub unbilled_periods: Vec<UnbilledPeriod>,
}

impl OutstandingBilling {
    /// Nothing is owed. The only clear answer.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.unpaid_invoices.is_empty() && self.unbilled_periods.is_empty()
    }

    /// Cash owed across the unpaid invoices. Unbilled usage contributes
    /// NOTHING here on purpose — it has no price until a finalize prices it,
    /// and a refusal that quoted a re-priced projection would be quoting a
    /// number no invoice will ever match.
    #[must_use]
    pub fn owed_cents(&self) -> i64 {
        self.unpaid_invoices.iter().map(|i| i.owed_cents).sum()
    }

    /// The currency the owed cash is denominated in, taken from the invoices
    /// themselves; the platform default when only unbilled usage remains.
    #[must_use]
    pub fn currency(&self) -> &str {
        self.unpaid_invoices
            .first()
            .map_or(crate::cron::billing_reconcile::BILLING_CURRENCY, |i| {
                i.currency.as_str()
            })
    }

    /// The action to name in the refusal. Cash already claimed outranks usage
    /// not yet claimed: settling the invoice is concrete and immediate, while
    /// the usage arm's remedy is the same attachment plus a wait for the sweep.
    #[must_use]
    pub fn remedy(&self) -> Option<BillingRemedy> {
        if !self.unpaid_invoices.is_empty() {
            Some(BillingRemedy::SettleInvoices)
        } else if self.unbilled_periods.is_empty() {
            None
        } else {
            Some(BillingRemedy::BillOutstandingUsage)
        }
    }
}

/// What `organization_id` still owes. THE predicate — every enforcement point
/// calls this one function rather than carrying its own SQL.
///
/// # The two arms, and what each deliberately does not count
///
/// **Unpaid finalized invoices.** `total_cents > Sum(invoice_payments)`, over
/// `status = 'finalized'` only. `draft` is excluded because `total_cents` stays
/// zero until the finalize UPDATE writes it, so a draft is not yet a claim;
/// `void` is excluded because voiding RELEASES the claim, which is the whole
/// point of the one transition `invoices_immutable()` permits on a finalized
/// row. A fully credit-covered invoice reads `total_cents` zero and records no
/// payment row at all, so it is settled by arithmetic rather than by a special
/// case. A chargeback appends a NEGATIVE `dispute_debit`, so it re-opens the
/// debt, which is right.
///
/// A held credit BALANCE is NOT subtracted. `credit::consume_at_finalize` is
/// the only writer of a `consumed` ledger row and it runs inside the finalize
/// transaction, so no code path will ever apply a balance to an invoice that is
/// already finalized. Netting it off here would forgive a debt nothing forgives.
///
/// A REFUND does not raise what is owed either: `cash_collected` reads
/// `invoice_payments` and refunds live in `zeroship.refunds`, so a
/// cash-refunded invoice reads settled. That is the tree's one oracle for
/// collected cash and the over-refund trigger inlines the same SELECT;
/// disagreeing with it here would make the refusal and the refund cap read
/// different balances.
///
/// **Unbilled closed-period usage.** A `usage_aggregates` row with usage, in a
/// period strictly BEFORE the current month, for which no non-void invoice
/// exists. The current month is excluded because it always has accrued usage
/// and never has an invoice — including it would mean no account could ever be
/// closed. Archived apps are NOT filtered, matching the reconciler, which bills
/// them on purpose. The arm is asked only under [`LocalInvoicing::Yes`].
///
/// # Errors
///
/// [`RegistryError::Database`] on a driver failure. There is no success value
/// that means "could not tell": every caller treats an error as a refusal,
/// because a deletion allowed on the strength of a check that did not run is
/// the failure this whole predicate exists to prevent.
pub async fn outstanding_billing<C: GenericClient + Sync>(
    conn: &C,
    organization_id: &str,
    invoicing: LocalInvoicing,
) -> Result<OutstandingBilling, RegistryError> {
    let invoice_rows = conn
        .query(
            "SELECT i.id, i.period::text AS period, i.currency, i.total_cents, \
                    cash.collected \
               FROM zeroship.invoices i \
               CROSS JOIN LATERAL ( \
                     SELECT COALESCE(SUM(p.amount_cents), 0)::bigint AS collected \
                       FROM zeroship.invoice_payments p \
                      WHERE p.invoice_id = i.id \
                    ) cash \
              WHERE i.organization_id = $1 \
                AND i.status = 'finalized' \
                AND i.total_cents > cash.collected \
              ORDER BY i.period, i.id",
            &[&organization_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let unpaid_invoices = invoice_rows
        .iter()
        .map(|r| {
            let total_cents: i64 = r.get("total_cents");
            let cash_collected_cents: i64 = r.get("collected");
            UnpaidInvoice {
                invoice_id: r.get("id"),
                period: r.get("period"),
                currency: r.get("currency"),
                total_cents,
                cash_collected_cents,
                owed_cents: total_cents - cash_collected_cents,
            }
        })
        .collect();

    let unbilled_periods = if invoicing.asks_about_unbilled_usage() {
        let usage_rows = conn
            .query(
                "SELECT u.period::text AS period, \
                        COUNT(DISTINCT u.app_id)::bigint AS apps, \
                        SUM(u.total)::bigint AS units \
                   FROM zeroship.usage_aggregates u \
                   JOIN zeroship.apps a ON a.id = u.app_id \
                  WHERE a.organization_id = $1 \
                    AND u.total > 0 \
                    AND u.period < date_trunc('month', NOW())::date \
                    AND NOT EXISTS ( \
                          SELECT 1 FROM zeroship.invoices i \
                           WHERE i.organization_id = a.organization_id \
                             AND i.period = u.period \
                             AND i.status <> 'void') \
                  GROUP BY u.period \
                  ORDER BY u.period",
                &[&organization_id],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        usage_rows
            .iter()
            .map(|r| UnbilledPeriod {
                period: r.get("period"),
                apps: r.get("apps"),
                units: r.get("units"),
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(OutstandingBilling {
        organization_id: organization_id.to_string(),
        unpaid_invoices,
        unbilled_periods,
    })
}

#[cfg(test)]
mod tests {
    // In-process cache unit tests (no DB) — peers of the DB-gated PR-7 suite.
    use super::*;

    #[test]
    fn projected_charge_cache_hit_within_ttl_does_not_advance_reprice_count() {
        let cache = ProjectedChargeCache::new(8);
        let app = Uuid::now_v7();
        let period = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let now = 1_000_000;

        // Empty → miss.
        assert!(cache.get(&app, period, now).is_none());
        // Simulate a compute: the live code bumps reprice_count then put()s.
        cache
            .reprice_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cache.put(app, period, 750, now);
        assert_eq!(cache.reprice_count(), 1);

        // A second call 30s later (within TTL) is a HIT — value served, no bump.
        let hit = cache.get(&app, period, now + 30).expect("within-ttl hit");
        assert_eq!(hit.projected_charge_cents, 750);
        assert_eq!(hit.as_of_unix, now, "as_of is the original compute instant");
        assert_eq!(cache.reprice_count(), 1, "a cache hit must NOT re-price");
    }

    #[test]
    fn projected_charge_cache_expires_after_ttl() {
        let cache = ProjectedChargeCache::new(8);
        let app = Uuid::now_v7();
        let period = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let now = 2_000_000;
        cache.put(app, period, 500, now);

        // Just past the TTL window ⇒ expired ⇒ miss (entry pruned).
        assert!(cache
            .get(&app, period, now + PROJECTED_CHARGE_TTL_SECS + 1)
            .is_none());
        // And a within-window read is still a hit.
        cache.put(app, period, 500, now);
        assert!(cache.get(&app, period, now + 1).is_some());
    }

    #[test]
    fn projected_charge_cache_evicts_at_capacity() {
        let cache = ProjectedChargeCache::new(2);
        let period = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let now = 3_000_000;
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = Uuid::now_v7();
        cache.put(a, period, 1, now);
        cache.put(b, period, 2, now);
        // Third distinct key over a cap of 2 evicts one prior entry; the map
        // never exceeds the cap.
        cache.put(c, period, 3, now);
        let live = [a, b, c]
            .iter()
            .filter(|k| cache.get(k, period, now).is_some())
            .count();
        assert!(live <= 2, "cache respects its capacity bound");
        assert!(cache.get(&c, period, now).is_some(), "the newest insert survives");
    }

    // -----------------------------------------------------------------------
    // The deletion predicate's derived fields (no DB; the SQL is bound by
    // `crates/zeroship-control/tests/deletion_owes_test.rs`).
    // -----------------------------------------------------------------------

    fn invoice(owed: i64) -> UnpaidInvoice {
        UnpaidInvoice {
            invoice_id: "inv_x".into(),
            period: "2026-01-01".into(),
            currency: "usd".into(),
            total_cents: owed,
            cash_collected_cents: 0,
            owed_cents: owed,
        }
    }

    fn usage() -> UnbilledPeriod {
        UnbilledPeriod {
            period: "2026-01-01".into(),
            apps: 1,
            units: 42,
        }
    }

    fn outstanding(invoices: Vec<UnpaidInvoice>, periods: Vec<UnbilledPeriod>) -> OutstandingBilling {
        OutstandingBilling {
            organization_id: "org_x".into(),
            unpaid_invoices: invoices,
            unbilled_periods: periods,
        }
    }

    /// An empty pair of lists is the ONLY settled answer, and each list alone
    /// is enough to refuse. The pairing is the point: a predicate that only
    /// looked at invoices would clear an organization whose usage was never
    /// billed, which is the shape a missing Stripe Customer produces.
    #[test]
    fn either_arm_alone_is_unsettled_and_only_both_empty_is_settled() {
        assert!(outstanding(vec![], vec![]).is_settled());
        assert!(!outstanding(vec![invoice(1000)], vec![]).is_settled());
        assert!(!outstanding(vec![], vec![usage()]).is_settled());
        assert!(!outstanding(vec![invoice(1000)], vec![usage()]).is_settled());
    }

    /// Unbilled usage carries units, never cents, so it must not move the
    /// owed figure a refusal quotes.
    #[test]
    fn owed_cents_sums_invoices_only() {
        assert_eq!(outstanding(vec![], vec![usage()]).owed_cents(), 0);
        assert_eq!(
            outstanding(vec![invoice(1000), invoice(250)], vec![usage()]).owed_cents(),
            1250
        );
    }

    /// The remedy is derived, ordered, and total. Cash already claimed wins:
    /// sending someone to wait for a sweep while an invoice is open would name
    /// the slower of two remedies.
    #[test]
    fn the_remedy_prefers_settling_claimed_cash() {
        assert_eq!(outstanding(vec![], vec![]).remedy(), None);
        assert_eq!(
            outstanding(vec![invoice(1)], vec![]).remedy(),
            Some(BillingRemedy::SettleInvoices)
        );
        assert_eq!(
            outstanding(vec![invoice(1)], vec![usage()]).remedy(),
            Some(BillingRemedy::SettleInvoices)
        );
        assert_eq!(
            outstanding(vec![], vec![usage()]).remedy(),
            Some(BillingRemedy::BillOutstandingUsage)
        );
    }

    /// Every remedy names a route, because a refusal with no next step is a
    /// dead end. The route named is the one that is wired end to end.
    #[test]
    fn every_remedy_names_a_route_that_exists() {
        for remedy in [
            BillingRemedy::SettleInvoices,
            BillingRemedy::BillOutstandingUsage,
        ] {
            assert!(
                remedy.instruction().contains("/billing/setup"),
                "{remedy:?} names no reachable route"
            );
        }
    }

    /// The currency comes from the invoice that owes it, and falls back to the
    /// platform's only when there is no invoice to read it from.
    #[test]
    fn the_currency_follows_the_invoice_that_owes() {
        let mut eur = invoice(500);
        eur.currency = "eur".into();
        assert_eq!(outstanding(vec![eur], vec![]).currency(), "eur");
        assert_eq!(
            outstanding(vec![], vec![usage()]).currency(),
            crate::cron::billing_reconcile::BILLING_CURRENCY
        );
    }

    /// The unbilled-usage arm is asked only of a stack that owns the local
    /// invoice rail. The control differing in one variable is the whole
    /// content of this test: under `No` there is no local invoice to be
    /// missing, so "usage without an invoice" would name every organization.
    #[test]
    fn only_a_local_invoicer_is_asked_about_unbilled_usage() {
        assert!(LocalInvoicing::Yes.asks_about_unbilled_usage());
        assert!(!LocalInvoicing::No.asks_about_unbilled_usage());
    }
}
