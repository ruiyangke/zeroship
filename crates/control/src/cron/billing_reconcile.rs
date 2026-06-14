//! Billing-reconcile cron (billing PR6, ISS-31, Stream-1).
//!
//! At month close (UTC), for each creator with billing enabled: sum the
//! creator's owned apps' usage aggregates for the CLOSED (previous) calendar
//! month, price each app via the PR4 plan catalog into invoice-item lines, push
//! those lines as Stripe **invoice items** on the creator's platform Customer
//! (`cus_…`), then create + finalize the invoice. Infra-cost billing ONLY — no
//! Connect, no `application_fee` (that is the separate Stream-2 epic).
//!
//! Creator→app resolution (decision D4 / H1): there is NO `apps.creator_id`
//! column; ownership flows through `zeroship.app_members WHERE role='owner'`. We
//! group owned apps by `user_id` ⇒ that user_id is the `creator_id`. Apps with
//! no owner row (e.g. the system console, 0036 `apps.system=true`) have no
//! billable creator and are SKIPPED.
//!
//! Idempotency — three airtight layers under at-least-once delivery:
//!   1. `billing_runs(creator_id, period_start)` PK, claimed BEFORE any Stripe
//!      call. A COMPLETED row (`stripe_invoice_id` NOT NULL) ⇒ already billed
//!      this period ⇒ skip entirely (no pricing, no Stripe call). A NULL-invoice
//!      row is a crash-window remnant we re-drive.
//!   2. The `billing_run_items(creator_id, period_start, app_id)` LEDGER is the
//!      DURABLE per-app double-bill guard, written CLAIM-THEN-CALL (C1): an
//!      intent row (`stripe_item_id = NULL`) is `INSERT`ed BEFORE `create_invoice_item`,
//!      then `UPDATE`d with the `ii_…` after. So the durable record PRECEDES the
//!      irreversible Stripe POST. On (re-)drive: a NOT-NULL `stripe_item_id` is
//!      skipped outright; a NULL row (intent recorded, outcome unknown — the
//!      crash-mid-call case) is reconciled by LOOKING UP the item via its
//!      deterministic `metadata.zs_item_key` (`find_invoice_item_by_key`) and
//!      adopting it if present, else posting fresh. This guarantees each app's
//!      item posts AT MOST ONCE even when Stripe's 24h Idempotency-Key window has
//!      expired (a >24h re-drive). The ledger + metadata lookup — not Stripe's
//!      key — is what makes the no-double-bill guarantee hold.
//!   3. The invoice is created (draft) and FINALIZED in two steps (C2): the draft
//!      id is persisted to `billing_runs.draft_invoice_id` the instant the draft
//!      exists, BEFORE finalize. A crash before finalize re-drives by finalizing
//!      THAT draft (which carries the real items) rather than creating a fresh
//!      empty draft — which a >24h create-key-expired re-drive would otherwise
//!      finalize at $0 (under-bill).
//!   4. A DETERMINISTIC Stripe `Idempotency-Key` per item/invoice derived from
//!      `(creator_id, app_id, period_start)` — belt-and-suspenders for the
//!      <24h replay case (Stripe returns the original object rather than
//!      creating a duplicate).
//!
//! Zero tokio: a `compio::time` interval; `compio-postgres`; `cyper` Stripe.
//! Multi-instance safety: a `pg_try_advisory_lock` around the sweep (the same
//! pattern PR5's `spend_reconcile` uses) so two control replicas don't
//! double-bill.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, TimeZone, Utc};
use uuid::Uuid;

use crate::metering::Metering;
use crate::plan_catalog::PlanCatalog;
use crate::pricing::{charge_cents, MetricWeights};
use crate::pricing_store::PricingStore;
use crate::registry::RegistryError;
use crate::stripe_client::{Period, StripeApi, StripeClient};
use crate::AppState;

/// Default tick cadence in seconds (~hourly). The closed-period claim is
/// idempotent, so a frequent tick is cheap: it no-ops once the previous month
/// is billed. Hourly bounds the lag between month-close and invoicing.
pub const DEFAULT_TICK_SECS: u64 = 3600;

/// Stable `pg_advisory_lock` key for the billing-reconcile sweep. Distinct from
/// the spend-sweep key. Two control instances racing this sweep would both try
/// to claim+bill; the per-period `billing_runs` PK already prevents a double
/// invoice, but the advisory lock avoids the wasted duplicate Stripe round-trips
/// and keeps the sweep single-flight fleet-wide. Arbitrary FIXED 64-bit constant
/// (derived from "zsbill01").
const BILLING_SWEEP_ADVISORY_LOCK_KEY: i64 = 0x7a73_6269_6c6c_0001;

/// Currency for infra-cost invoices (v1: USD only).
const BILLING_CURRENCY: &str = "usd";

/// Compute the unix-seconds start of the calendar month BEFORE the month
/// containing `now_unix` (UTC). This is the CLOSED period the reconciler bills:
/// on any tick during month M, we bill month M-1.
#[must_use]
pub fn previous_period_start_unix(now_unix: i64) -> i64 {
    let dt = Utc.timestamp_opt(now_unix, 0).single().unwrap_or_else(Utc::now);
    let (year, month) = if dt.month() == 1 {
        (dt.year() - 1, 12)
    } else {
        (dt.year(), dt.month() - 1)
    };
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .map_or(now_unix, |d| d.timestamp())
}

/// The exclusive end of the period that starts at `period_start_unix` (the start
/// of the NEXT calendar month). Used as the invoice-item `period[end]`.
#[must_use]
pub fn period_end_unix(period_start_unix: i64) -> i64 {
    let dt = Utc
        .timestamp_opt(period_start_unix, 0)
        .single()
        .unwrap_or_else(Utc::now);
    let (year, month) = if dt.month() == 12 {
        (dt.year() + 1, 1)
    } else {
        (dt.year(), dt.month() + 1)
    };
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .map_or(period_start_unix, |d| d.timestamp())
}

/// Deterministic Stripe `Idempotency-Key` for the per-app invoice-ITEM create.
/// Stable for a fixed `(creator, app, period)` so a retry replays the same item.
#[must_use]
pub fn invoice_item_idempotency_key(creator_id: &Uuid, app_id: &Uuid, period_start_unix: i64) -> String {
    format!("billitem:{creator_id}:{app_id}:{period_start_unix}")
}

/// Deterministic Stripe `Idempotency-Key` for the per-run invoice create.
/// Stable for a fixed `(creator, period)`.
#[must_use]
pub fn invoice_idempotency_key(creator_id: &Uuid, period_start_unix: i64) -> String {
    format!("billrun:{creator_id}:{period_start_unix}")
}

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then sleeps
/// `tick_secs`. A transient error is logged + swallowed so the task survives
/// (mirrors `spend_reconcile` / `audit_retention`).
#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control billing_reconcile cron starting");
    loop {
        match tick(&state).await {
            Ok(n) if n > 0 => {
                tracing::info!(billed = n, "control billing_reconcile sweep completed");
            }
            Ok(_) => { /* steady state; nothing to bill */ }
            Err(e) => {
                tracing::error!(error = %e, "control billing_reconcile tick failed");
            }
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// Run one reconcile sweep against the live Stripe client built from `state`.
/// Returns the number of creators billed (a freshly-claimed `billing_runs` row
/// that resulted in a finalized invoice).
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    let stripe = StripeClient::new(
        crate::SecretString::new(state.stripe_secret_key.expose_secret().to_string()),
    )
    .with_base_url(state.stripe_base_url.clone());
    tick_with(state, &stripe, Utc::now().timestamp()).await
}

/// Sweep core, parameterized on the [`StripeApi`] and the wall-clock `now` (unix
/// seconds) so an integration test can drive a single deterministic tick against
/// a mock-Stripe server, or a unit test against a recording fake.
///
/// Takes the advisory lock for the whole sweep (multi-instance safety), bills
/// the CLOSED previous month, and returns the number of creators billed.
#[allow(clippy::future_not_send)]
pub async fn tick_with<S: StripeApi>(
    state: &AppState,
    stripe: &S,
    now_unix: i64,
) -> Result<usize, RegistryError> {
    let period_start = previous_period_start_unix(now_unix);

    // Multi-instance safety: single-flight the sweep fleet-wide. A loser skips
    // this tick (the per-period `billing_runs` PK still prevents a double bill,
    // but the lock avoids duplicate Stripe round-trips).
    let lock_conn = state.registry.conn().await?;
    let got = lock_conn
        .query(
            "SELECT pg_try_advisory_lock($1) AS locked",
            &[&BILLING_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await?;
    let acquired = got.first().is_some_and(|r| r.get::<_, bool>("locked"));
    if !acquired {
        tracing::debug!("billing_reconcile: advisory lock held by another instance — skipping tick");
        return Ok(0);
    }

    let result = sweep(state, stripe, period_start).await;

    if let Err(e) = lock_conn
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&BILLING_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await
    {
        tracing::warn!(error = %e, "billing_reconcile: advisory unlock failed (frees on conn drop)");
    }

    result
}

/// The advisory-lock-protected body: group owned apps by creator, bill each.
#[allow(clippy::future_not_send)]
async fn sweep<S: StripeApi>(
    state: &AppState,
    stripe: &S,
    period_start: i64,
) -> Result<usize, RegistryError> {
    // Creator→apps via ownership (H1): app_members WHERE role='owner'. Apps with
    // no owner row are absent here and thus skipped. One owner per app by
    // construction (0031), but we group defensively in case of fan-out.
    let conn = state.registry.conn().await?;
    // DISTINCT ON (app_id): an app must map to AT MOST ONE owner row so a
    // (data-integrity) fan-out of multiple role='owner' rows can never bill the
    // same app twice (MINOR-9). One owner per app by construction (0031); the
    // DISTINCT ON makes that defensive rather than load-bearing.
    let owner_rows = conn
        .query(
            "SELECT DISTINCT ON (m.app_id) m.user_id AS creator_id, m.app_id \
             FROM zeroship.app_members m \
             WHERE m.role = 'owner' \
             ORDER BY m.app_id, m.user_id",
            &[],
        )
        .await?;
    // BTreeMap for deterministic creator ordering (stable invoice sequencing).
    let mut apps_by_creator: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for row in &owner_rows {
        let creator_id: Uuid = row.get("creator_id");
        let app_id: Uuid = row.get("app_id");
        apps_by_creator.entry(creator_id).or_default().push(app_id);
    }

    let metering = Metering::new(state.registry.clone());
    let catalog = PlanCatalog::new(state.registry.clone());

    // Compute-unit pricing (Refactor B): load the GLOBAL cost model + the
    // default FX ONCE per tick (tiny global tables), then bill each app's
    // closed-period usage as integer CU × the plan's effective FX. The invoice
    // shape is UNCHANGED — one item per app = `charge_cents(...).total_cents`.
    let pricing = PricingStore::new(state.registry.clone());
    let weights = pricing.weights().await?;
    let default_fx = pricing.default_fx_pico_cents_per_unit().await?;

    let mut billed = 0usize;

    for (creator_id, app_ids) in &apps_by_creator {
        match bill_creator(
            state, stripe, &metering, &catalog, &weights, default_fx, creator_id, app_ids,
            period_start,
        )
        .await
        {
            Ok(true) => billed += 1,
            Ok(false) => { /* nothing to bill / already billed / no customer */ }
            // MAJOR-2: a missing global default FX means the platform cannot
            // price ANY inheriting plan — this is NOT a per-creator hiccup. Abort
            // the WHOLE sweep (bill no one) so we never emit a mix of correct and
            // silently-$0 invoices. Fail closed.
            Err(e @ RegistryError::FxUnresolved) => {
                tracing::error!(
                    creator_id = %creator_id,
                    error = %e,
                    "billing_reconcile: global default FX missing — ABORTING sweep (no creator billed)"
                );
                return Err(e);
            }
            Err(e) => {
                // A per-creator failure must not abort the whole sweep — log and
                // continue so one creator's Stripe hiccup doesn't starve others.
                tracing::error!(
                    creator_id = %creator_id,
                    error = %e,
                    "billing_reconcile: failed to bill creator — continuing"
                );
            }
        }
    }
    Ok(billed)
}

/// Bill ONE creator for the closed period. Returns `Ok(true)` if a fresh invoice
/// was finalized this call, `Ok(false)` for a no-op (zero charge, already billed,
/// or no saved customer).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::future_not_send)]
pub(crate) async fn bill_creator<S: StripeApi>(
    state: &AppState,
    stripe: &S,
    metering: &Metering,
    catalog: &PlanCatalog,
    weights: &MetricWeights,
    default_fx: Option<u64>,
    creator_id: &Uuid,
    app_ids: &[Uuid],
    period_start: i64,
) -> Result<bool, RegistryError> {
    // Integer-exact period PK (MAJOR-4): bind TIMESTAMPTZ directly rather than
    // round-tripping the unix-seconds key through f64, so a claim and any
    // re-drive resolve to the BYTE-IDENTICAL period_start.
    let period_ts: DateTime<Utc> = Utc
        .timestamp_opt(period_start, 0)
        .single()
        .ok_or_else(|| RegistryError::Database(format!("invalid period_start {period_start}")))?;

    let conn = state.registry.conn().await?;

    // MAJOR-6: short-circuit BEFORE any pricing. If a COMPLETED run row already
    // exists (stripe_invoice_id NOT NULL) this period is fully billed — do no
    // pricing work at all. A NULL-invoice row is a crash-window remnant we still
    // need to re-drive, so we fall through to pricing in that case only.
    let existing = conn
        .query(
            "SELECT stripe_invoice_id FROM zeroship.billing_runs \
             WHERE creator_id = $1 AND period_start = $2",
            &[creator_id, &period_ts],
        )
        .await?;
    let run_exists = !existing.is_empty();
    let invoice_done = existing
        .first()
        .and_then(|r| r.get::<_, Option<String>>("stripe_invoice_id"))
        .is_some();
    if invoice_done {
        return Ok(false);
    }

    // Resolve the creator's Customer; a creator with no saved payment identity is
    // skipped. MAJOR-5: if such a creator HAS usage we will surface a warn below
    // (silent under-bill is revenue lost invisibly).
    let customer = match state.stripe_store.get_customer(*creator_id).await {
        Ok(Some(c)) => Some(c),
        Ok(None) => None,
        Err(e) => return Err(RegistryError::Database(format!("get_customer: {e}"))),
    };

    // Price each owned app's closed-period usage.
    let mut lines: Vec<(Uuid, String, u64)> = Vec::new();
    let mut total_cents: u64 = 0;
    for app_id in app_ids {
        // Resolve the app's plan (FK into the catalog). A missing/poison plan is
        // skipped, not fatal.
        let plan_id = lookup_plan_id(state, app_id).await?;
        let Some(plan_id) = plan_id else { continue };
        let Some(plan) = catalog.get(&plan_id).await? else {
            tracing::warn!(app_id = %app_id, plan_id = %plan_id, "billing_reconcile: plan not in catalog — skipping app");
            continue;
        };
        let usage = metering.period_totals(app_id, period_start).await?;
        let price = plan.price.with_effective_fx(default_fx);
        // MAJOR-1/MAJOR-2: pricing is fallible and MUST NOT silently clamp.
        //   * UnresolvedFx (global default FX missing) ⇒ the platform cannot
        //     price ⇒ propagate so the sweep ABORTS (see `sweep`), never a
        //     base-only $0 invoice.
        //   * ComputeUnitOverflow ⇒ a hard error that skips THIS creator (the
        //     per-creator loop catches it + warns), never a clamped bill —
        //     matching the cents→i64 hard-error posture below.
        let breakdown = match charge_cents(&price, &usage, weights) {
            Ok(b) => b,
            Err(crate::pricing::PricingError::UnresolvedFx) => {
                tracing::error!(
                    app_id = %app_id,
                    plan_id = %plan_id,
                    "billing_reconcile: global default FX missing — cannot price; ABORTING sweep"
                );
                return Err(RegistryError::FxUnresolved);
            }
            Err(e @ crate::pricing::PricingError::ComputeUnitOverflow { .. }) => {
                return Err(RegistryError::Database(format!(
                    "billing_reconcile: {e} — refusing to bill app {app_id}"
                )));
            }
        };
        if breakdown.total_cents == 0 {
            continue;
        }
        let desc = format!("Infra usage — app {app_id} — {}", month_label(period_start));
        lines.push((*app_id, desc, breakdown.total_cents));
        total_cents = total_cents.saturating_add(breakdown.total_cents);
    }

    if total_cents == 0 {
        // Nothing to bill this period for this creator.
        return Ok(false);
    }

    // MAJOR-5: usage exists but no saved Customer ⇒ we CANNOT bill. Surface it
    // loudly (a missing-customer marker) instead of dropping revenue at debug!.
    let Some(customer) = customer else {
        tracing::warn!(
            creator_id = %creator_id,
            total_cents,
            billing_event = "missing_customer_with_usage",
            "billing_reconcile: creator has billable usage but no saved Stripe Customer — NOT billed"
        );
        return Ok(false);
    };

    // MAJOR-3: money MUST NOT silently clamp. An overflow here is a hard error
    // that skips this creator (the per-creator loop catches it + warns), never a
    // clamp to i64::MAX.
    let amount_i64 = i64::try_from(total_cents).map_err(|_| {
        RegistryError::Database(format!(
            "billing_reconcile: total_cents {total_cents} exceeds i64::MAX — refusing to clamp"
        ))
    })?;

    // Layer 1: claim the run BEFORE any Stripe call (only if not already
    // present). 0 rows ⇒ already claimed by a prior tick that may have crashed
    // mid-Stripe — we re-drive using the per-app ledger to avoid double-posting.
    if !run_exists {
        conn.execute(
            "INSERT INTO zeroship.billing_runs (creator_id, period_start, amount_cents) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (creator_id, period_start) DO NOTHING",
            &[creator_id, &period_ts, &amount_i64],
        )
        .await?;
    } else {
        tracing::warn!(
            creator_id = %creator_id,
            "billing_reconcile: re-driving a run with NULL stripe_invoice_id (crash-window recovery)"
        );
    }

    // CRIT-1 (claim-then-call): the per-app ledger is the DURABLE double-bill
    // guard, and the DURABLE INTENT must PRECEDE the irreversible Stripe POST.
    // We load the current ledger state for this (creator, period):
    //   * `stripe_item_id IS NOT NULL` ⇒ the item DEFINITELY posted ⇒ skip.
    //   * `stripe_item_id IS NULL`     ⇒ intent was recorded but the outcome is
    //     unknown (a prior drive crashed between the POST and the ledger commit).
    //     We do NOT blindly re-POST: within 24h the deterministic Idempotency-Key
    //     replays the same item; past 24h (key expired) we LOOK UP the item by
    //     its deterministic metadata key and adopt it if present, else post fresh.
    let posted_rows = conn
        .query(
            "SELECT app_id, stripe_item_id FROM zeroship.billing_run_items \
             WHERE creator_id = $1 AND period_start = $2",
            &[creator_id, &period_ts],
        )
        .await?;
    // app_id → Some(stripe_item_id) if confirmed-posted, None if intent-only.
    let mut ledger_state: std::collections::HashMap<Uuid, Option<String>> =
        std::collections::HashMap::new();
    for r in &posted_rows {
        ledger_state.insert(r.get::<_, Uuid>("app_id"), r.get::<_, Option<String>>("stripe_item_id"));
    }

    let period = Period {
        start: period_start,
        end: period_end_unix(period_start),
    };
    for (app_id, desc, amount) in &lines {
        match ledger_state.get(app_id) {
            // Confirmed posted in a prior drive — skip outright (no re-POST).
            Some(Some(_)) => continue,
            // Intent recorded but post unconfirmed (crash mid-call) — adopt or
            // post fresh below.
            Some(None) => {}
            // No ledger row yet — write the durable INTENT row BEFORE the Stripe
            // call (claim-then-call). A crash after this INSERT but before the
            // POST leaves a NULL-`stripe_item_id` row we reconcile on re-drive.
            None => {
                let item_amount = i64::try_from(*amount).map_err(|_| {
                    RegistryError::Database(format!(
                        "billing_reconcile: item amount {amount} exceeds i64::MAX — refusing to clamp"
                    ))
                })?;
                conn.execute(
                    "INSERT INTO zeroship.billing_run_items \
                       (creator_id, period_start, app_id, stripe_item_id, amount_cents) \
                     VALUES ($1, $2, $3, NULL, $4) \
                     ON CONFLICT (creator_id, period_start, app_id) DO NOTHING",
                    &[creator_id, &period_ts, app_id, &item_amount],
                )
                .await?;
            }
        }

        let item_key = invoice_item_idempotency_key(creator_id, app_id, period_start);

        // For an intent-only re-drive, first try to ADOPT an already-posted item
        // by its deterministic metadata key (closes the >24h window where the
        // Idempotency-Key no longer dedupes). If found, we did NOT re-POST.
        let mut item_id: Option<String> = None;
        if matches!(ledger_state.get(app_id), Some(None)) {
            item_id = stripe
                .find_invoice_item_by_key(&customer, &item_key)
                .await
                .map_err(|e| RegistryError::Database(format!("find_invoice_item_by_key: {e}")))?;
            if item_id.is_some() {
                tracing::warn!(
                    creator_id = %creator_id,
                    app_id = %app_id,
                    "billing_reconcile: adopted an already-posted invoice item on re-drive (intent recovery)"
                );
            }
        }

        // Not adopted ⇒ POST it. Within 24h the deterministic Idempotency-Key
        // makes this replay-safe; past 24h the intent row + the metadata lookup
        // above already ruled out an existing item, so a fresh POST is correct.
        let item_id = match item_id {
            Some(id) => id,
            None => stripe
                .create_invoice_item(
                    &customer, *amount, BILLING_CURRENCY, desc, period, &item_key, &item_key,
                )
                .await
                .map_err(|e| RegistryError::Database(format!("create_invoice_item: {e}")))?,
        };

        // Confirm the post in the ledger (fills the NULL intent row's
        // stripe_item_id). A crash AFTER the POST but BEFORE this UPDATE leaves
        // the intent row NULL — the next drive adopts via the metadata lookup, so
        // the item is still posted AT MOST ONCE.
        conn.execute(
            "UPDATE zeroship.billing_run_items SET stripe_item_id = $4 \
             WHERE creator_id = $1 AND period_start = $2 AND app_id = $3",
            &[creator_id, &period_ts, app_id, &item_id],
        )
        .await?;
    }

    // C2 (persist-draft-before-finalize): create the draft invoice (which sweeps
    // the customer's pending items), persist its id IMMEDIATELY, THEN finalize.
    // On a re-drive, if a draft id is already persisted, finalize THAT existing
    // draft (which carries the real line items) rather than creating a new empty
    // draft — which a >24h create-key-expired re-drive would otherwise finalize
    // at $0 (under-bill).
    let existing_draft: Option<String> = conn
        .query(
            "SELECT draft_invoice_id FROM zeroship.billing_runs \
             WHERE creator_id = $1 AND period_start = $2",
            &[creator_id, &period_ts],
        )
        .await?
        .first()
        .and_then(|r| r.get::<_, Option<String>>("draft_invoice_id"));

    let draft_id = match existing_draft {
        Some(id) => {
            tracing::warn!(
                creator_id = %creator_id,
                "billing_reconcile: re-finalizing an existing draft invoice (crash-before-finalize recovery)"
            );
            id
        }
        None => {
            let invoice_key = invoice_idempotency_key(creator_id, period_start);
            let id = stripe
                .create_invoice(&customer, &creator_id.to_string(), &invoice_key)
                .await
                .map_err(|e| RegistryError::Database(format!("create_invoice: {e}")))?;
            // Persist the draft id BEFORE finalize. A crash here (post-create,
            // pre-finalize) is recovered by the re-drive finding this id above.
            conn.execute(
                "UPDATE zeroship.billing_runs SET draft_invoice_id = $3 \
                 WHERE creator_id = $1 AND period_start = $2",
                &[creator_id, &period_ts, &id],
            )
            .await?;
            id
        }
    };

    let invoice_id = stripe
        .finalize_invoice(&draft_id)
        .await
        .map_err(|e| RegistryError::Database(format!("finalize_invoice: {e}")))?;

    // Record the finalized Stripe invoice id + amount on the (already-claimed)
    // run row.
    conn.execute(
        "UPDATE zeroship.billing_runs \
         SET stripe_invoice_id = $3, amount_cents = $4 \
         WHERE creator_id = $1 AND period_start = $2",
        &[creator_id, &period_ts, &invoice_id, &amount_i64],
    )
    .await?;

    Ok(true)
}

/// Resolve the apps owned by ONE creator (the per-creator slice of the same
/// `app_members WHERE role='owner'` ownership query `sweep` runs fleet-wide).
/// Used by [`crate::metering::provider::native::NativeProvider::invoice`] so the
/// per-creator provider verb bills exactly the creator's owned apps. Behaviour
/// matches `sweep`'s grouping (DISTINCT ON keeps an app at most once).
#[allow(clippy::future_not_send)]
pub(crate) async fn owned_app_ids(
    state: &AppState,
    creator_id: &Uuid,
) -> Result<Vec<Uuid>, RegistryError> {
    let conn = state.registry.conn().await?;
    let rows = conn
        .query(
            "SELECT DISTINCT ON (m.app_id) m.app_id \
             FROM zeroship.app_members m \
             WHERE m.role = 'owner' AND m.user_id = $1 \
             ORDER BY m.app_id, m.user_id",
            &[creator_id],
        )
        .await?;
    Ok(rows.iter().map(|r| r.get::<_, Uuid>("app_id")).collect())
}

/// Resolve an app's `plan_id`. Returns `None` if the app row is gone.
#[allow(clippy::future_not_send)]
pub(crate) async fn lookup_plan_id(state: &AppState, app_id: &Uuid) -> Result<Option<String>, RegistryError> {
    let conn = state.registry.conn().await?;
    let rows = conn
        .query("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[app_id])
        .await?;
    Ok(rows.first().map(|r| r.get::<_, String>("plan_id")))
}

/// `YYYY-MM` label for a period start (used in the invoice-item description).
fn month_label(period_start_unix: i64) -> String {
    let dt = Utc
        .timestamp_opt(period_start_unix, 0)
        .single()
        .unwrap_or_else(Utc::now);
    format!("{:04}-{:02}", dt.year(), dt.month())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_period_is_prior_calendar_month() {
        // Mid-June 2026 → previous period starts 2026-05-01 UTC.
        let mid_june = Utc.with_ymd_and_hms(2026, 6, 13, 12, 0, 0).unwrap().timestamp();
        let ps = previous_period_start_unix(mid_june);
        assert_eq!(ps, Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap().timestamp());
    }

    #[test]
    fn previous_period_wraps_january_to_december() {
        let mid_jan = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap().timestamp();
        let ps = previous_period_start_unix(mid_jan);
        assert_eq!(ps, Utc.with_ymd_and_hms(2025, 12, 1, 0, 0, 0).unwrap().timestamp());
    }

    #[test]
    fn period_end_is_next_month_start() {
        let may = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap().timestamp();
        assert_eq!(period_end_unix(may), Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap().timestamp());
        let dec = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap().timestamp();
        assert_eq!(period_end_unix(dec), Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap().timestamp());
    }

    #[test]
    fn idempotency_keys_are_deterministic_and_distinct() {
        let creator = Uuid::nil();
        let app_a = Uuid::from_u128(1);
        let app_b = Uuid::from_u128(2);
        let p = 1_700_000_000i64;
        // Stable for a fixed tuple.
        assert_eq!(
            invoice_item_idempotency_key(&creator, &app_a, p),
            invoice_item_idempotency_key(&creator, &app_a, p),
        );
        // Distinct per app and per period.
        assert_ne!(
            invoice_item_idempotency_key(&creator, &app_a, p),
            invoice_item_idempotency_key(&creator, &app_b, p),
        );
        assert_ne!(
            invoice_item_idempotency_key(&creator, &app_a, p),
            invoice_item_idempotency_key(&creator, &app_a, p + 1),
        );
        // Invoice key format.
        assert_eq!(invoice_idempotency_key(&creator, p), format!("billrun:{creator}:{p}"));
    }

    #[test]
    fn month_label_formats_year_month() {
        let may = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap().timestamp();
        assert_eq!(month_label(may), "2026-05");
    }

    #[test]
    fn default_tick_is_hourly() {
        assert_eq!(DEFAULT_TICK_SECS, 3600);
    }

    // -- injected-StripeApi unit (no PG): prove the trait seam records the
    //    invoice-item lines + the deterministic keys a sweep would emit, using a
    //    recording fake instead of the cyper client. (blueprint PR6 (d) unit.)

    use std::cell::RefCell;

    use crate::pricing::{charge_cents, MetricWeight, MetricWeights, PlanPrice};
    use crate::stripe_client::{Period, StripeApi};
    use crate::stripe_store::StripeError;

    #[derive(Default)]
    struct RecordingStripe {
        items: RefCell<Vec<(String, u64, String)>>, // (customer, amount, idempotency_key)
        invoices: RefCell<Vec<(String, String)>>,   // (customer, idempotency_key)
    }

    impl StripeApi for RecordingStripe {
        async fn create_customer(&self, _email: &str, _creator: &str) -> Result<String, StripeError> {
            Ok("cus_fake".to_string())
        }
        async fn create_checkout_setup_session(
            &self,
            _c: &str,
            _ok: &str,
            _cancel: &str,
        ) -> Result<String, StripeError> {
            Ok("https://fake/session".to_string())
        }
        async fn create_invoice_item(
            &self,
            customer: &str,
            amount_cents: u64,
            _currency: &str,
            _description: &str,
            _period: Period,
            idempotency_key: &str,
            _lookup_key: &str,
        ) -> Result<String, StripeError> {
            self.items.borrow_mut().push((
                customer.to_string(),
                amount_cents,
                idempotency_key.to_string(),
            ));
            Ok(format!("ii_{}", self.items.borrow().len()))
        }
        async fn find_invoice_item_by_key(
            &self,
            _customer: &str,
            _lookup_key: &str,
        ) -> Result<Option<String>, StripeError> {
            Ok(None)
        }
        async fn create_invoice(
            &self,
            customer: &str,
            _creator_id: &str,
            idempotency_key: &str,
        ) -> Result<String, StripeError> {
            self.invoices
                .borrow_mut()
                .push((customer.to_string(), idempotency_key.to_string()));
            Ok("in_fake".to_string())
        }
        async fn finalize_invoice(&self, _invoice_id: &str) -> Result<String, StripeError> {
            Ok("in_fake".to_string())
        }
    }

    #[compio::test]
    async fn injected_stripe_api_records_line_and_deterministic_keys() {
        // The line logic the sweep applies: charge_cents → one invoice item per
        // app, then one finalized invoice. Drive it against a RECORDING fake
        // (no PG, no cyper) to pin the per-app amount + the deterministic keys.
        let creator = Uuid::from_u128(0xAA);
        let app = Uuid::from_u128(0xBB);
        let period = previous_period_start_unix(
            Utc.with_ymd_and_hms(2026, 6, 13, 0, 0, 0).unwrap().timestamp(),
        );

        // CU pricing: weight 1 CU/request, FX = 1 cent/CU ⇒ 750 requests = 750c.
        let price = PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(crate::pricing::FX_SCALE as u64),
            spend_limit_default_cents: 0,
        };
        let mut weights = MetricWeights::new();
        weights.insert("requests".to_string(), MetricWeight { units_per_op: 1, per_units: 1 });
        let mut usage = std::collections::HashMap::new();
        usage.insert("requests".to_string(), 750i64);
        let breakdown = charge_cents(&price, &usage, &weights).expect("charge");
        assert_eq!(breakdown.total_cents, 750);

        let fake = RecordingStripe::default();
        let item_key = invoice_item_idempotency_key(&creator, &app, period);
        fake.create_invoice_item(
            "cus_fake",
            breakdown.total_cents,
            "usd",
            "infra",
            Period { start: period, end: period_end_unix(period) },
            &item_key,
            &item_key,
        )
        .await
        .unwrap();
        let invoice_key = invoice_idempotency_key(&creator, period);
        let draft = fake.create_invoice("cus_fake", &creator.to_string(), &invoice_key).await.unwrap();
        fake.finalize_invoice(&draft).await.unwrap();

        let items = fake.items.borrow();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1, 750, "the per-app amount equals charge_cents total");
        assert_eq!(items[0].2, format!("billitem:{creator}:{app}:{period}"));
        assert_eq!(fake.invoices.borrow()[0].1, format!("billrun:{creator}:{period}"));
    }
}
