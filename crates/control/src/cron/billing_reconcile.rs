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
//! Idempotency — three airtight layers under at-least-once delivery (mapped onto
//! the provider-agnostic invoice model: `invoices` + `invoice_lines` +
//! `billing_provider_refs` / `billing_line_provider_refs`):
//!   1. `invoices(creator_id, period)` UNIQUE, claimed BEFORE any Stripe call via
//!      `INSERT … 'draft' ON CONFLICT DO NOTHING`. A `status='finalized'` row ⇒
//!      already billed this period ⇒ skip entirely (no pricing, no Stripe call).
//!      A `status='draft'` row is a crash-window remnant we re-drive.
//!   2. The `invoice_lines(invoice_id, app_id)` LEDGER is the DURABLE per-app
//!      double-bill guard, written CLAIM-THEN-CALL (C1): the line (carrying the
//!      frozen charge SNAPSHOT) is `INSERT`ed BEFORE `create_invoice_item`, and a
//!      `billing_line_provider_refs(provider='stripe', ref_kind='invoice_item')`
//!      row (a REAL composite FK → the line) is written AFTER. So the durable
//!      record PRECEDES the irreversible Stripe POST. On (re-)drive: a line whose
//!      provider-ref EXISTS is skipped outright (== old `stripe_item_id NOT
//!      NULL`); a line with NO provider-ref (intent recorded, outcome unknown —
//!      the crash-mid-call case) is reconciled by LOOKING UP the item via its
//!      deterministic `metadata.zs_item_key` (`find_invoice_item_by_key`) and
//!      adopting it if present, else posting fresh. This guarantees each app's
//!      item posts AT MOST ONCE even when Stripe's 24h Idempotency-Key window has
//!      expired (a >24h re-drive). The line + ref + metadata lookup — not Stripe's
//!      key — is what makes the no-double-bill guarantee hold.
//!   3. The invoice is created (draft) and FINALIZED in distinct steps (C2): the
//!      draft id is persisted to a `billing_provider_refs(ref_kind='draft_invoice')`
//!      row the instant the draft exists, BEFORE finalize. A crash before finalize
//!      re-drives by finalizing THAT draft (which carries the real items) rather
//!      than creating a fresh empty draft — which a >24h create-key-expired
//!      re-drive would otherwise finalize at $0 (under-bill). FINALIZE writes
//!      subtotal/credit/tax/total/status/finalized_at in ONE UPDATE so the
//!      `invoice_total_balances` CHECK never sees a half-written row.
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

use chrono::{Datelike, TimeZone, Utc};
use uuid::Uuid;

use crate::metering::{period_date, Metering};
use crate::plan_catalog::PlanCatalog;
use crate::pricing::{charge_cents, MetricWeight, MetricWeights};
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
/// to claim+bill; the per-period `invoices(creator_id, period)` UNIQUE claim
/// already prevents a double invoice, but the advisory lock avoids the wasted
/// duplicate Stripe round-trips and keeps the sweep single-flight fleet-wide.
/// Arbitrary FIXED 64-bit constant (derived from "zsbill01").
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

/// Deterministic Stripe `Idempotency-Key` for the per-SEGMENT invoice-ITEM create.
/// Stable for a fixed `(creator, app, period, segment_no)` so a retry replays the
/// same item. round 4, CRITICAL-1: the key includes `segment_no` — an app posts
/// N+1 items in a period (one per plan segment), and a segment-blind key would
/// dedup them to a SINGLE Stripe item (only segment 0 posts, the rest silently
/// dropped — an under-bill). The N=0 degenerate path passes `segment_no = 0`, so
/// its key is `billitem:{creator}:{app}:{period}:0` — a stable superset of the
/// pre-PR-4 shape (the `:0` suffix is the only change).
#[must_use]
pub fn invoice_item_idempotency_key(
    creator_id: &Uuid,
    app_id: &Uuid,
    period_start_unix: i64,
    segment_no: i16,
) -> String {
    format!("billitem:{creator_id}:{app_id}:{period_start_unix}:{segment_no}")
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
/// Returns the number of creators billed (a freshly-claimed `invoices` row that
/// resulted in a finalized invoice).
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
    // this tick (the per-period `invoices(creator_id, period)` UNIQUE claim still
    // prevents a double bill, but the lock avoids duplicate Stripe round-trips).
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
    // Bulk pre-filter (single fleet-wide query): only apps with at least one
    // `usage_aggregates` row for the CLOSED period can produce a non-zero charge —
    // every other app prices to `total_cents == 0` and is skipped inside
    // `bill_creator` anyway. Dropping them BEFORE the per-creator/per-app loop
    // means the per-app pricing reads (each of which opens a fresh,
    // SCRAM-authenticated PG connection — `Registry` has no pool) run only over
    // apps that actually accrued usage, not over every app ever owned. A creator
    // left with no active app would have billed `total_cents == 0` (a no-op), so
    // skipping them is observably identical (no invoice either way). This keeps
    // the sweep cost O(apps-with-usage) instead of O(every-app-ever-owned).
    let period = period_date(period_start);
    let active_app_rows = conn
        .query(
            "SELECT DISTINCT app_id FROM zeroship.usage_aggregates WHERE period = $1::date",
            &[&period],
        )
        .await?;
    let active_apps: std::collections::HashSet<Uuid> =
        active_app_rows.iter().map(|r| r.get::<_, Uuid>("app_id")).collect();

    // BTreeMap for deterministic creator ordering (stable invoice sequencing).
    // Only apps that accrued usage this period are retained (see pre-filter above).
    let mut apps_by_creator: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for row in &owner_rows {
        let app_id: Uuid = row.get("app_id");
        if !active_apps.contains(&app_id) {
            continue;
        }
        let creator_id: Uuid = row.get("creator_id");
        apps_by_creator.entry(creator_id).or_default().push(app_id);
    }

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
            state, stripe, &catalog, &weights, default_fx, creator_id, app_ids,
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
    catalog: &PlanCatalog,
    weights: &MetricWeights,
    default_fx: Option<u64>,
    creator_id: &Uuid,
    app_ids: &[Uuid],
    period_start: i64,
) -> Result<bool, RegistryError> {
    // `period` is the first-of-month `billing_period` DATE (the claim key). Bound
    // via `$N::date` on every write (the domain param OID rejects a bare
    // NaiveDate); reads need no cast.
    let period = period_date(period_start);

    // `mut` so the finalize→provider-ref pair can run in ONE `conn.transaction()`
    // (M1). Every read/UPSERT before that still borrows `&conn` immutably.
    let mut conn = state.registry.conn().await?;

    // MAJOR-6: short-circuit BEFORE any pricing. A `status='finalized'` invoice
    // for (creator, period) means this period is fully billed — do no pricing
    // work at all (== old `stripe_invoice_id NOT NULL`). A `draft` row is a
    // crash-window remnant we re-drive, so we fall through to pricing then.
    // Scope to the ACTIVE (non-void) invoice. After a void+reissue (billing-ops
    // PR-1) the same (creator, period) can have BOTH a voided audit row AND a live
    // non-void row; the partial unique index `WHERE status <> 'void'` guarantees AT
    // MOST ONE non-void row, so this read is single and the void audit rows are
    // ignored (a re-drive must never pick up the void or it would mis-decide
    // invoice_done / re-drive the wrong id).
    let existing = conn
        .query(
            "SELECT id, status FROM zeroship.invoices \
             WHERE creator_id = $1 AND period = $2::date AND status <> 'void'",
            &[creator_id, &period],
        )
        .await?;
    let existing_invoice_id: Option<String> = existing.first().map(|r| r.get::<_, String>("id"));
    let invoice_done = existing
        .first()
        .map(|r| r.get::<_, String>("status"))
        .as_deref()
        == Some("finalized");
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

    // Price each owned app's closed-period usage, capturing the SNAPSHOT inputs
    // (usage, applied weights, included_units, resolved FX, base fee, authoritative
    // amount) so a finalized line replays bit-for-bit via charge_cents.
    //
    // PR-4 (full usage-segment proration): an app with N plan-change events in the
    // period splits into N+1 SEGMENTS. Each segment is priced under ITS OWN plan
    // over the cumulative-snapshot usage DELTA and emitted as a SEPARATE line. The
    // no-change case degenerates to exactly one segment_no=0 line (full period,
    // current plan) — byte-for-byte today's behaviour.
    //
    // C1 (replay-faithful snapshot): freeze the FULL weights map that was ACTUALLY
    // PASSED to `charge_cents` — weights are GLOBAL (segment-agnostic). Each segment
    // line's `usage_snapshot` is its own DELTA, so the segments' snapshots telescope
    // back to the full-period total.
    let weights_snapshot: std::collections::BTreeMap<String, MetricWeight> =
        weights.iter().map(|(m, w)| (m.clone(), *w)).collect();
    let mut lines: Vec<BilledLine> = Vec::new();
    let mut total_cents: u64 = 0;
    for app_id in app_ids {
        // Resolve the app's CURRENT plan (FK into the catalog) — the tail's source
        // of truth (MAJOR-4) and the single segment's plan on the no-change path. A
        // missing/poison plan is skipped, not fatal.
        let plan_id = lookup_plan_id_on(&conn, app_id).await?;
        let Some(current_plan_id) = plan_id else { continue };
        if catalog.get(&current_plan_id).await?.is_none() {
            tracing::warn!(app_id = %app_id, plan_id = %current_plan_id, "billing_reconcile: plan not in catalog — skipping app");
            continue;
        }

        // The period-end cumulative totals (the END of the last segment).
        let period_end_totals = Metering::period_totals_on(&conn, app_id, period_start).await?;

        // The period's plan-change events, effective_at order. Each opens a segment.
        let change_rows = conn
            .query(
                "SELECT to_plan_id, from_plan_id, effective_at, usage_at_change \
                 FROM zeroship.plan_change_events \
                 WHERE app_id = $1 AND period = $2::date \
                 ORDER BY effective_at, id",
                &[app_id, &period],
            )
            .await?;
        let mut period_changes: Vec<crate::proration::PlanChange> = Vec::with_capacity(change_rows.len());
        for r in &change_rows {
            let usage_json: serde_json::Value = r.get("usage_at_change");
            // MINOR-4: a malformed `usage_at_change` MUST NOT silently become `{}` —
            // that would zero the segment START snapshot and over-count the whole
            // segment (every metric billed from 0 instead of its true cumulative
            // start). Propagate the parse error so the per-creator loop in `sweep`
            // logs it and SKIPS this creator's billing this tick, rather than
            // emitting an over-bill off a silently-empty snapshot.
            let usage_at_change: std::collections::HashMap<String, i64> =
                serde_json::from_value(usage_json).map_err(|e| {
                    RegistryError::Database(format!(
                        "billing_reconcile: corrupt usage_at_change for app {app_id} — \
                         refusing to price off an empty snapshot (would over-bill): {e}"
                    ))
                })?;
            period_changes.push(crate::proration::PlanChange {
                to_plan_id: r.get::<_, String>("to_plan_id"),
                effective_at: r.get::<_, chrono::DateTime<Utc>>("effective_at"),
                usage_at_change,
            });
        }
        // The plan running at period START (segment 0's plan): the first change's
        // `from_plan_id`. That column is NULL for an initial plan assignment (no
        // prior plan existed) — in which case there is no distinct pre-change plan,
        // so segment 0 falls back to the current plan. It is also the current plan
        // when there were NO changes this period (`change_rows` empty ⇒ `.first()` is
        // None). Both fallbacks resolve via `unwrap_or_else`.
        let prior_plan_id: String = change_rows
            .first()
            .and_then(|r| r.get::<_, Option<String>>("from_plan_id"))
            .unwrap_or_else(|| current_plan_id.clone());

        // Resolve the (FX-effective) PlanPrice for every plan a segment may use:
        // the prior plan, the current plan, and each change's to-plan.
        let mut plan_prices: std::collections::HashMap<String, crate::pricing::PlanPrice> =
            std::collections::HashMap::new();
        let mut needed: std::collections::HashSet<String> = std::collections::HashSet::new();
        needed.insert(prior_plan_id.clone());
        needed.insert(current_plan_id.clone());
        for ch in &period_changes {
            needed.insert(ch.to_plan_id.clone());
        }
        for pid in &needed {
            if let Some(p) = catalog.get(pid).await? {
                plan_prices.insert(pid.clone(), p.price.with_effective_fx(default_fx));
            }
        }

        let segments = crate::proration::build_segments_with_prior(
            period_start,
            &prior_plan_id,
            &period_changes,
            &period_end_totals,
            &current_plan_id,
            &plan_prices,
        );

        for seg in &segments {
            // Price the segment under ITS OWN plan: the day-pro-rated base fee +
            // quota and the segment plan's FX, over the segment's usage DELTA.
            let seg_price = crate::pricing::PlanPrice {
                base_fee_cents: seg.base_fee_cents,
                included_units: seg.included_units,
                // None ⇒ unresolved FX ⇒ charge_cents fails closed (no silent $0).
                fx_pico_cents_per_unit: seg.fx_pico_cents_per_unit,
                spend_limit_default_cents: 0,
            };
            // MAJOR-1/MAJOR-2: pricing is fallible and MUST NOT silently clamp.
            //   * UnresolvedFx ⇒ propagate so the sweep ABORTS, never a $0 invoice.
            //   * ComputeUnitOverflow ⇒ a hard error that skips THIS creator.
            let breakdown = match charge_cents(&seg_price, &seg.usage_delta, weights) {
                Ok(b) => b,
                Err(crate::pricing::PricingError::UnresolvedFx) => {
                    tracing::error!(
                        app_id = %app_id,
                        plan_id = %seg.plan_id,
                        segment_no = seg.segment_no,
                        "billing_reconcile: global default FX missing — cannot price; ABORTING sweep"
                    );
                    return Err(RegistryError::FxUnresolved);
                }
                Err(e @ crate::pricing::PricingError::ComputeUnitOverflow { .. }) => {
                    return Err(RegistryError::Database(format!(
                        "billing_reconcile: {e} — refusing to bill app {app_id} segment {}",
                        seg.segment_no
                    )));
                }
            };
            if breakdown.total_cents == 0 {
                // A $0 segment posts no Stripe item / line (mirrors the per-app
                // skip today). The N=0 path skips a $0 app exactly as before.
                continue;
            }
            let desc = segment_description(*app_id, seg, period_start, segments.len());
            lines.push(BilledLine {
                app_id: *app_id,
                segment_no: seg.segment_no,
                plan_id: seg.plan_id.clone(),
                desc,
                amount: breakdown.total_cents,
                usage: seg.usage_delta.clone(),
                weights_snapshot: weights_snapshot.clone(),
                included_units: seg.included_units,
                // charge_cents succeeded above ⇒ the FX resolved to Some; freeze it.
                fx_pico_cents_per_unit: seg.fx_pico_cents_per_unit.unwrap_or(0),
                base_fee_cents: seg.base_fee_cents,
            });
            total_cents = total_cents.saturating_add(breakdown.total_cents);
        }
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

    // Layer 1: claim the invoice (draft) BEFORE any Stripe call. ON CONFLICT
    // (creator_id, period) DO NOTHING is the no-double-bill claim. We then read
    // back the id either way (a re-drive reuses the existing draft).
    let invoice_id = match existing_invoice_id {
        Some(id) => {
            tracing::warn!(
                creator_id = %creator_id,
                "billing_reconcile: re-driving an existing draft invoice (crash-window recovery)"
            );
            id
        }
        None => {
            let new_id = zeroship_core::typed_id::new_invoice_id();
            // ON CONFLICT targets the PARTIAL unique index `invoices_active_period_claim`
            // (WHERE status <> 'void') the 0042 reshape introduced — NOT the old
            // unconditional UNIQUE (which is gone). The `WHERE status <> 'void'` on the
            // conflict clause names the partial index's predicate so a voided prior
            // invoice does NOT collide: a corrected invoice can reissue into the released
            // period slot (billing-ops PR-1, gap #26 C).
            conn.execute(
                "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
                 VALUES ($1, $2, $3::date, 'draft') \
                 ON CONFLICT (creator_id, period) WHERE status <> 'void' DO NOTHING",
                &[&new_id, creator_id, &period],
            )
            .await?;
            // ON CONFLICT may have no-op'd if a concurrent drive claimed it first
            // (the advisory lock makes this rare, but be exact): re-read the id. Scope to
            // the ACTIVE (non-void) row — a voided invoice for this period still exists as
            // an audit row, but the live claim is the non-void one.
            conn.query(
                "SELECT id FROM zeroship.invoices \
                 WHERE creator_id = $1 AND period = $2::date AND status <> 'void'",
                &[creator_id, &period],
            )
            .await?
            .first()
            .map(|r| r.get::<_, String>("id"))
            .ok_or_else(|| {
                RegistryError::Database("billing_reconcile: invoice claim vanished".to_string())
            })?
        }
    };

    // CRIT-1 (claim-then-call): the per-SEGMENT `invoice_lines` row is the DURABLE
    // double-bill guard, and the DURABLE INTENT (the line + its frozen snapshot)
    // must PRECEDE the irreversible Stripe POST. Presence of a
    // `billing_line_provider_refs` row (a real composite FK → the line) == old
    // `stripe_item_id NOT NULL`. round 4, CRITICAL-1: post-`0051` the guards are
    // keyed `(app_id, segment_no)` — keyed by `app_id` ALONE, N segments of an app
    // would collide to ONE Stripe item and only segment 0 would post (under-bill).
    let posted_rows = conn
        .query(
            "SELECT app_id, segment_no FROM zeroship.billing_line_provider_refs \
             WHERE invoice_id = $1 AND provider = 'stripe' AND ref_kind = 'invoice_item'",
            &[&invoice_id],
        )
        .await?;
    let posted: std::collections::HashSet<(Uuid, i16)> = posted_rows
        .iter()
        .map(|r| (r.get::<_, Uuid>("app_id"), r.get::<_, i16>("segment_no")))
        .collect();
    // Which (app, segment) lines already have an intent (snapshot) row written?
    let line_rows = conn
        .query(
            "SELECT app_id, segment_no FROM zeroship.invoice_lines WHERE invoice_id = $1",
            &[&invoice_id],
        )
        .await?;
    let line_exists: std::collections::HashSet<(Uuid, i16)> = line_rows
        .iter()
        .map(|r| (r.get::<_, Uuid>("app_id"), r.get::<_, i16>("segment_no")))
        .collect();

    // MAJOR-1: reconcile the EXISTING draft's (app, segment) set against the FRESHLY
    // built one BEFORE posting. A prior crashed drive may have posted/recorded MORE
    // segments than this re-drive builds (a zero-day-merge / period-end-totals
    // interaction can shrink the segment count). Those stale higher-segment lines +
    // their already-posted Stripe items would otherwise be swept by the draft
    // invoice, so the finalized subtotal (this drive's lines) would DISAGREE with the
    // Stripe total — an over-charge. Draft lines are MUTABLE until finalize (the
    // immutability trigger fires only on a finalized parent), so we delete the orphans
    // here: drop the Stripe item (adopting an un-ref'd-but-possibly-posted item via
    // its deterministic metadata key first), then the provider-ref, then the line.
    let fresh_keys: std::collections::HashSet<(Uuid, i16)> =
        lines.iter().map(|l| (l.app_id, l.segment_no)).collect();
    let orphans: Vec<(Uuid, i16)> = line_exists
        .union(&posted)
        .copied()
        .filter(|k| !fresh_keys.contains(k))
        .collect();
    for (orphan_app, orphan_seg) in &orphans {
        // 1. Remove the Stripe item. If the orphan has a confirmed provider-ref we
        //    know its external id; otherwise (line-only intent) it may STILL have been
        //    posted (crash between POST and ref-insert), so look it up by its
        //    deterministic metadata key and delete it if present.
        let item_id: Option<String> = if posted.contains(&(*orphan_app, *orphan_seg)) {
            conn.query(
                "SELECT external_id FROM zeroship.billing_line_provider_refs \
                 WHERE invoice_id = $1 AND app_id = $2 AND segment_no = $3 \
                   AND provider = 'stripe' AND ref_kind = 'invoice_item'",
                &[&invoice_id, orphan_app, orphan_seg],
            )
            .await?
            .first()
            .map(|r| r.get::<_, String>("external_id"))
        } else {
            // line-only intent (no confirmed ref): it may STILL have been posted
            // (crash between POST and ref-insert), so look it up by its key.
            let orphan_key =
                invoice_item_idempotency_key(creator_id, orphan_app, period_start, *orphan_seg);
            stripe
                .find_invoice_item_by_key(&customer, &orphan_key)
                .await
                .map_err(|e| RegistryError::Database(format!("find_invoice_item_by_key: {e}")))?
        };
        if let Some(id) = item_id {
            stripe
                .delete_invoice_item(&id)
                .await
                .map_err(|e| RegistryError::Database(format!("delete_invoice_item: {e}")))?;
            tracing::warn!(
                creator_id = %creator_id,
                app_id = %orphan_app,
                segment_no = orphan_seg,
                "billing_reconcile: deleted an orphaned Stripe invoice item (re-drive built fewer segments)"
            );
        }
        // 2. Drop the provider-ref (composite-FK child) THEN the line. ON DELETE
        //    CASCADE on the FK means deleting the line would also drop the ref, but
        //    we delete the ref explicitly first so the order is unambiguous.
        conn.execute(
            "DELETE FROM zeroship.billing_line_provider_refs \
             WHERE invoice_id = $1 AND app_id = $2 AND segment_no = $3",
            &[&invoice_id, orphan_app, orphan_seg],
        )
        .await?;
        conn.execute(
            "DELETE FROM zeroship.invoice_lines \
             WHERE invoice_id = $1 AND app_id = $2 AND segment_no = $3",
            &[&invoice_id, orphan_app, orphan_seg],
        )
        .await?;
    }

    let period_window = Period {
        start: period_start,
        end: period_end_unix(period_start),
    };
    for line in &lines {
        let app_id = &line.app_id;
        let key = (*app_id, line.segment_no);
        // Confirmed posted in a prior drive — skip outright (no re-POST).
        if posted.contains(&key) {
            continue;
        }
        let intent_only = line_exists.contains(&key);

        // SNAPSHOT-ONTO-(SEGMENT-)LINE before the POST: write (or refresh, while
        // still draft) the line carrying the frozen charge inputs + authoritative
        // amount. The line trigger keeps these mutable until the parent finalizes.
        let item_amount = i64::try_from(line.amount).map_err(|_| {
            RegistryError::Database(format!(
                "billing_reconcile: item amount {} exceeds i64::MAX — refusing to clamp",
                line.amount
            ))
        })?;
        let included_i64 = i64::try_from(line.included_units).unwrap_or(i64::MAX);
        let fx_i64 = i64::try_from(line.fx_pico_cents_per_unit).unwrap_or(i64::MAX);
        let base_i64 = i64::try_from(line.base_fee_cents).unwrap_or(i64::MAX);
        let usage_json = serde_json::to_value(&line.usage)
            .map_err(|e| RegistryError::Database(format!("usage_snapshot serialize: {e}")))?;
        let weights_json = serde_json::to_value(&line.weights_snapshot)
            .map_err(|e| RegistryError::Database(format!("weights_snapshot serialize: {e}")))?;
        conn.execute(
            "INSERT INTO zeroship.invoice_lines \
               (invoice_id, app_id, segment_no, plan_id, included_units, \
                fx_pico_cents_per_unit, base_fee_cents, \
                amount_cents, usage_snapshot, weights_snapshot) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (invoice_id, app_id, segment_no) DO UPDATE SET \
               plan_id = EXCLUDED.plan_id, \
               included_units = EXCLUDED.included_units, \
               fx_pico_cents_per_unit = EXCLUDED.fx_pico_cents_per_unit, \
               base_fee_cents = EXCLUDED.base_fee_cents, \
               amount_cents = EXCLUDED.amount_cents, \
               usage_snapshot = EXCLUDED.usage_snapshot, \
               weights_snapshot = EXCLUDED.weights_snapshot",
            &[
                &invoice_id,
                app_id,
                &line.segment_no,
                &line.plan_id,
                &included_i64,
                &fx_i64,
                &base_i64,
                &item_amount,
                &usage_json,
                &weights_json,
            ],
        )
        .await?;

        let item_key =
            invoice_item_idempotency_key(creator_id, app_id, period_start, line.segment_no);

        // For an intent-only re-drive, first try to ADOPT an already-posted item
        // by its deterministic metadata key (closes the >24h window where the
        // Idempotency-Key no longer dedupes). If found, we did NOT re-POST.
        let mut item_id: Option<String> = None;
        if intent_only {
            item_id = stripe
                .find_invoice_item_by_key(&customer, &item_key)
                .await
                .map_err(|e| RegistryError::Database(format!("find_invoice_item_by_key: {e}")))?;
            if item_id.is_some() {
                tracing::warn!(
                    creator_id = %creator_id,
                    app_id = %app_id,
                    segment_no = line.segment_no,
                    "billing_reconcile: adopted an already-posted invoice item on re-drive (intent recovery)"
                );
            }
        }

        // Not adopted ⇒ POST it. Within 24h the deterministic Idempotency-Key
        // makes this replay-safe; past 24h the line intent + the metadata lookup
        // above already ruled out an existing item, so a fresh POST is correct.
        let item_id = match item_id {
            Some(id) => id,
            None => stripe
                .create_invoice_item(
                    &customer, line.amount, BILLING_CURRENCY, &line.desc, period_window, &item_key,
                    &item_key,
                )
                .await
                .map_err(|e| RegistryError::Database(format!("create_invoice_item: {e}")))?,
        };

        // Confirm the post: write the line provider-ref (the real composite FK ⇒
        // a malformed (invoice, app, segment) key is rejected at write). A crash
        // AFTER the POST but BEFORE this INSERT leaves the line with no ref — the
        // next drive adopts via the metadata lookup, so the item posts AT MOST ONCE.
        conn.execute(
            "INSERT INTO zeroship.billing_line_provider_refs \
               (invoice_id, app_id, segment_no, provider, ref_kind, external_id) \
             VALUES ($1, $2, $3, 'stripe', 'invoice_item', $4) \
             ON CONFLICT (invoice_id, app_id, segment_no, provider, ref_kind) DO NOTHING",
            &[&invoice_id, app_id, &line.segment_no, &item_id],
        )
        .await?;
    }

    // C2 (persist-draft-before-finalize): create the draft invoice (which sweeps
    // the customer's pending items), persist its id IMMEDIATELY (as a
    // `draft_invoice` provider-ref), THEN finalize. On a re-drive, if a draft id
    // is already persisted, finalize THAT existing draft (which carries the real
    // line items) rather than creating a new empty draft — which a >24h
    // create-key-expired re-drive would otherwise finalize at $0 (under-bill).
    let existing_draft: Option<String> = conn
        .query(
            "SELECT external_id FROM zeroship.billing_provider_refs \
             WHERE invoice_id = $1 AND provider = 'stripe' AND ref_kind = 'draft_invoice'",
            &[&invoice_id],
        )
        .await?
        .first()
        .map(|r| r.get::<_, String>("external_id"));

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
            // pre-finalize) is recovered by the re-drive finding this ref above.
            conn.execute(
                "INSERT INTO zeroship.billing_provider_refs \
                   (invoice_id, provider, ref_kind, external_id) \
                 VALUES ($1, 'stripe', 'draft_invoice', $2) \
                 ON CONFLICT (invoice_id, provider, ref_kind) DO NOTHING",
                &[&invoice_id, &id],
            )
            .await?;
            id
        }
    };

    // M2 (re-finalize converges): a crash AFTER Stripe finalized but BEFORE our
    // local finalize-UPDATE leaves Stripe-finalized + DB-draft. The re-drive
    // re-calls `finalize_invoice(&draft_id)`; Stripe rejects finalizing an
    // already-finalized invoice with a 4xx (`error.code = invoice_already_finalized`).
    // Treat that as SUCCESS — the invoice IS finalized on Stripe, and finalize does
    // NOT change the invoice id, so the provider invoice id == `draft_id`. We then
    // proceed to the local finalize-UPDATE so a re-drive CONVERGES instead of
    // error-looping forever (which would strand the DB at 'draft').
    let provider_invoice_id = match stripe.finalize_invoice(&draft_id).await {
        Ok(id) => id,
        Err(e) if is_already_finalized(&e) => {
            tracing::warn!(
                creator_id = %creator_id,
                draft_id = %draft_id,
                "billing_reconcile: invoice already finalized on Stripe (crash-after-finalize \
                 recovery) — converging the local finalize"
            );
            draft_id.clone()
        }
        Err(e) => return Err(RegistryError::Database(format!("finalize_invoice: {e}"))),
    };

    // M1 (atomic finalize→provider-ref): the local finalize-UPDATE (status →
    // 'finalized') and the `billing_provider_refs(ref_kind='invoice')` INSERT must
    // commit TOGETHER. As two separate autocommits, a crash between them leaves a
    // finalized invoice with NO 'invoice' ref; the re-drive short-circuits on
    // status='finalized' (returns Ok(false)) and NEVER backfills it, so
    // `native.rs::lookup_invoice_id` returns None forever (un-auditable). Wrapping
    // both in ONE transaction makes that partial state impossible. Both are local
    // PG writes (the Stripe POST already happened above), so the txn holds no
    // network call.
    //
    // FINALIZE-IN-ONE-UPDATE is preserved: the UPDATE writes
    // subtotal/credit/tax/total/status/finalized_at in ONE statement so the
    // `invoice_total_balances` CHECK (total = subtotal − credit + tax) never sees a
    // half-written row. The immutability trigger then freezes the invoice + its lines.
    //
    // CREDIT-APPLY AT FINALIZE (billing-ops PR-2, design flow A): just before the
    // finalize UPDATE, INSIDE this same txn, consume the creator's available credit
    // OLDEST-FIRST against the SUBTOTAL (credit applied BEFORE tax, matching the
    // balance CHECK's `total = subtotal − credit + tax` ordering). One `consumed`
    // entry per drawn grant is appended, keyed to THIS invoice id so a reconcile
    // re-run (crash-window re-drive of the same draft claim) recomputes the same
    // applied credit and NEVER double-consumes (see `consume_at_finalize`).
    //
    // TAX AT FINALIZE (billing-ops PR-5, design flow E): AFTER credit is applied and
    // BEFORE the finalize UPDATE — still inside this same txn — call the `TaxProvider`
    // seam over the POST-CREDIT subtotal (`subtotal − applied_credit`, the amount the
    // creator actually owes; tax is computed on the post-credit base, matching the
    // balance CHECK's `total = subtotal − credit + tax` ordering). The result is frozen
    // into `tax_cents` in the ONE-statement finalize UPDATE (replacing today's hard-wired
    // `0`), so `total = subtotal − credit + tax` holds without the CHECK ever seeing a
    // half-written row. Tax is computed ONCE per invoice over the summed segment subtotal
    // (the multi-segment proration composes: `amount_i64` is the sum of every app/segment
    // line). `NativeTaxProvider` returns 0 at launch (USD), so `total = subtotal − credit`
    // is unchanged; enabling Stripe Tax later is a provider swap (`automatic_tax`), not a
    // schema change — `tax_cents` already exists.
    let tx = conn.transaction().await?;
    let credit = crate::credit::consume_at_finalize(
        &tx, creator_id, &invoice_id, amount_i64, BILLING_CURRENCY,
    )
    .await?;
    let credit_i64 = credit.applied_cents;
    let taxable_base_cents = (amount_i64 - credit_i64).max(0);
    let tax = state
        .tax_provider
        .compute_tax(&crate::tax::TaxContext {
            creator_id: *creator_id,
            taxable_base_cents,
            currency: BILLING_CURRENCY,
            period,
        })
        .await
        .map_err(RegistryError::from)?;
    let tax_i64 = tax.tax_cents;
    let total_i64 = amount_i64 - credit_i64 + tax_i64;
    tx.execute(
        "UPDATE zeroship.invoices \
         SET subtotal_cents = $2, credit_cents = $3, tax_cents = $5, total_cents = $4, \
             status = 'finalized', finalized_at = NOW(), updated_at = NOW() \
         WHERE id = $1",
        &[&invoice_id, &amount_i64, &credit_i64, &total_i64, &tax_i64],
    )
    .await?;
    // Record the finalized provider invoice id (the seam — core invoices carry no
    // provider ids) in the SAME txn as the finalize-UPDATE.
    tx.execute(
        "INSERT INTO zeroship.billing_provider_refs \
           (invoice_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'invoice', $2) \
         ON CONFLICT (invoice_id, provider, ref_kind) DO NOTHING",
        &[&invoice_id, &provider_invoice_id],
    )
    .await?;
    tx.commit().await?;

    Ok(true)
}

/// True if a [`StripeError`] from `finalize_invoice` means the invoice was ALREADY
/// finalized on Stripe (a re-drive of a crash-after-finalize window). Stripe
/// returns a 4xx with `error.code = "invoice_already_finalized"` in that case;
/// treating it as success lets the local finalize converge (M2). Matched on the
/// machine-readable code, not the human message.
fn is_already_finalized(e: &crate::stripe_store::StripeError) -> bool {
    matches!(
        e,
        crate::stripe_store::StripeError::Api { code: Some(code), .. }
            if code == "invoice_already_finalized"
    )
}

/// One priced per-SEGMENT line during a reconcile, carrying the frozen charge
/// inputs (the snapshot) so a finalized invoice replays bit-for-bit. PR-4: an app
/// with N plan-change events emits N+1 of these (one per segment); the no-change
/// path emits exactly one at `segment_no = 0`.
struct BilledLine {
    app_id: Uuid,
    segment_no: i16,
    plan_id: String,
    desc: String,
    amount: u64,
    /// The segment's usage DELTA (`max(0, end−start)` per metric), NOT a cumulative.
    usage: std::collections::HashMap<String, i64>,
    weights_snapshot: std::collections::BTreeMap<String, MetricWeight>,
    included_units: u64,
    fx_pico_cents_per_unit: u64,
    base_fee_cents: u64,
}

/// Per-segment Stripe line-item description (MISSING-3). The no-change path (a
/// single full-period segment) keeps today's description with no day-span suffix;
/// a prorated segment carries its plan + half-open day-span so the creator's
/// Stripe-hosted invoice reads correctly per segment, e.g.
/// `"Infra usage — app <id> — 2026-05 (pln_… days 11–30)"`.
fn segment_description(
    app_id: Uuid,
    seg: &crate::proration::BilledSegment,
    period_start_unix: i64,
    segment_count: usize,
) -> String {
    let label = month_label(period_start_unix);
    if segment_count <= 1 {
        format!("Infra usage — app {app_id} — {label}")
    } else {
        // Half-open `[start_day, end_day)` rendered as an inclusive day range.
        let last_day = seg.end_day.saturating_sub(1);
        format!(
            "Infra usage — app {app_id} — {label} ({}, days {}–{})",
            seg.plan_id, seg.start_day, last_day
        )
    }
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

/// Resolve an app's `plan_id` on a BORROWED connection (the caller already holds
/// one — the per-app reconcile path and the metering-export sweep), so it avoids a
/// fresh per-query connection handshake. Returns `None` if the app row is gone.
#[allow(clippy::future_not_send)]
pub(crate) async fn lookup_plan_id_on<C: compio_postgres::GenericClient + Sync>(
    conn: &C,
    app_id: &Uuid,
) -> Result<Option<String>, RegistryError> {
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
            invoice_item_idempotency_key(&creator, &app_a, p, 0),
            invoice_item_idempotency_key(&creator, &app_a, p, 0),
        );
        // Distinct per app and per period.
        assert_ne!(
            invoice_item_idempotency_key(&creator, &app_a, p, 0),
            invoice_item_idempotency_key(&creator, &app_b, p, 0),
        );
        assert_ne!(
            invoice_item_idempotency_key(&creator, &app_a, p, 0),
            invoice_item_idempotency_key(&creator, &app_a, p + 1, 0),
        );
        // round 4, CRITICAL-1: distinct per SEGMENT — else N segments collide to
        // one Stripe item and only segment 0 posts (under-bill).
        assert_ne!(
            invoice_item_idempotency_key(&creator, &app_a, p, 0),
            invoice_item_idempotency_key(&creator, &app_a, p, 1),
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
        async fn delete_invoice_item(&self, _item_id: &str) -> Result<(), StripeError> {
            Ok(())
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
        async fn create_meter_event(
            &self,
            _event_name: &str,
            _stripe_customer_id: &str,
            _value: u64,
            _identifier: &str,
            _timestamp: i64,
        ) -> Result<(), StripeError> {
            Ok(())
        }
        async fn meter_event_summary(
            &self,
            _meter_id: &str,
            _stripe_customer_id: &str,
            _start_time: i64,
            _end_time: i64,
        ) -> Result<u64, StripeError> {
            Ok(0)
        }
        // Stream-2 Connect verbs (G1) — not exercised by the infra reconciler;
        // stubbed so the fake satisfies the trait.
        async fn create_connect_account(
            &self,
            _email: &str,
            _creator_id: &str,
            _country: &str,
        ) -> Result<String, StripeError> {
            Ok("acct_fake".to_string())
        }
        async fn create_account_link(
            &self,
            _account_id: &str,
            _refresh_url: &str,
            _return_url: &str,
        ) -> Result<String, StripeError> {
            Ok("https://fake/onboard".to_string())
        }
        async fn retrieve_account(
            &self,
            account_id: &str,
        ) -> Result<crate::stripe_client::ConnectAccount, StripeError> {
            Ok(crate::stripe_client::ConnectAccount {
                id: account_id.to_string(),
                charges_enabled: true,
                payouts_enabled: true,
                details_submitted: true,
                creator_id: None,
            })
        }
        async fn create_connect_payment_intent(
            &self,
            _connected_account: &str,
            _amount_cents: u64,
            _currency: &str,
            _application_fee_cents: u64,
            _description: &str,
            _idempotency_key: &str,
        ) -> Result<crate::stripe_client::ConnectPaymentIntent, StripeError> {
            Ok(crate::stripe_client::ConnectPaymentIntent {
                id: "pi_fake".to_string(),
                client_secret: None,
            })
        }
        async fn create_refund(
            &self,
            _provider_invoice_id: &str,
            _amount_cents: u64,
            _currency: &str,
            _idempotency_key: &str,
        ) -> Result<String, StripeError> {
            Ok("re_fake".to_string())
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
        let item_key = invoice_item_idempotency_key(&creator, &app, period, 0);
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
        assert_eq!(items[0].2, format!("billitem:{creator}:{app}:{period}:0"));
        assert_eq!(fake.invoices.borrow()[0].1, format!("billrun:{creator}:{period}"));
    }
}
