//! Metering-export cron (M-Stripe, blueprint §M5/§M9): the periodic sweep that
//! forwards each app's CURRENT-period compute units to an external meter
//! (Stripe Billing Meters / OpenMeter). Spawned ONLY for an export provider —
//! NEVER for native (where `report_usage` is a no-op and spawning it would be
//! pure waste, blueprint §M5 table).
//!
//! ## The crux — idempotent DELTA export
//!
//! Stripe meter events are SUMMED on Stripe's side, so the cron must push the
//! CU CONSUMED SINCE THE LAST EXPORT (the delta), not the cumulative total, and
//! must never double-push on a cron re-run / crash. Per app per tick:
//!
//!   1. Derive the app's CURRENT cumulative CU from the LOCAL ledger:
//!      `total_units(weights, period_totals(app, period_start))`. This is the
//!      EXACT derivation the spend cap uses (`pricing::total_units`) — the CU is
//!      the single source of truth, re-derived from the raw, re-weightable
//!      `usage_aggregates` (NO CU column is added).
//!   2. Read the per-`(app, period)` high-water `exported_units` from
//!      `metering_exports` (0043). `delta = current − exported`.
//!   3. `delta == 0` ⇒ nothing new ⇒ SKIP (the no-op that makes a re-run/crash
//!      safe — a second sweep over an unchanged total pushes nothing).
//!   4. Push the delta via `provider.report_usage(customer, app, period, delta,
//!      identifier)`. The `identifier` is DETERMINISTIC from
//!      `(app, period, exported→current window)` so a Stripe-side replay of the
//!      SAME window dedups too (defense in depth).
//!   5. ONLY on a successful push, UPSERT `exported_units = current`. A crash
//!      between the push and this write leaves the OLD high-water, so the next
//!      sweep re-pushes the SAME window — same `identifier` ⇒ Stripe dedups ⇒
//!      no double-count.
//!
//! The local high-water is the PRIMARY guard; the deterministic identifier is
//! the secondary (Stripe-side) guard. Together: never double-push.
//!
//! Enforcement is untouched: this sweep is export-only and never feeds
//! `spend.rs`/`enforce.rs`. Zero tokio: a `compio::time` interval;
//! `compio-postgres`; the provider's `cyper` client. Multi-instance safety: a
//! `pg_try_advisory_lock` (a NEW distinct key) around the sweep.
//!
//! ## Append-only metering — no clawback (m3)
//!
//! Stripe Billing Meters is ADDITIVE: once CU is exported it is NEVER clawed
//! back. The export only ever pushes a NON-NEGATIVE delta (`current − already`,
//! floored at 0); it never pushes a negative correction. So a DOWNWARD re-weight
//! mid-period (lowering a metric's `units_per_op`, shrinking `current`) does NOT
//! refund Stripe — the already-exported CU stays counted. Operators must treat
//! metric weights as APPEND-ONLY within a billing period; re-price by opening a
//! new period, not by reducing weights mid-period.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use crate::cron::billing_reconcile::{lookup_plan_id_on, period_end_unix};
use crate::metering::{current_period_start_unix, Metering};
use crate::plan_catalog::PlanCatalog;
use crate::pricing::total_units;
use crate::pricing_store::PricingStore;
use crate::registry::RegistryError;
use crate::AppState;
use crate::metering::provider::{BillingPeriod, CreatorBilling, CustomerRef};

/// Default tick cadence in seconds (~hourly), matching the billing sweep. The
/// delta export is idempotent (the high-water + the deterministic identifier),
/// so a frequent tick is cheap: an unchanged total pushes nothing. Hourly
/// bounds the lag between usage accrual and the external meter.
pub const DEFAULT_TICK_SECS: u64 = 3600;

/// Stable `pg_advisory_lock` key for the metering-export sweep. DISTINCT from
/// the billing (`zsbill01`) and spend (`zsspnd1`) keys so the three sweeps
/// never contend on one lock. Arbitrary FIXED 64-bit constant (from "zsmexp01").
const EXPORT_SWEEP_ADVISORY_LOCK_KEY: i64 = 0x7a73_6d65_7870_0001;

/// Deterministic dedup `identifier` for ONE meter push: the `(app, period,
/// exported→current window)` tuple. Stable for a fixed window so a re-push of
/// the SAME window (a crash before the high-water write) replays the SAME
/// identifier and Stripe dedups it. A NEW window (usage grew → a different
/// `current`) yields a NEW identifier so its delta is counted once.
#[must_use]
pub fn export_identifier(
    app_id: &Uuid,
    period_start_unix: i64,
    exported_units: u64,
    current_units: u64,
) -> String {
    format!("export:{app_id}:{period_start_unix}:{exported_units}:{current_units}")
}

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then sleeps
/// `tick_secs`. A transient error is logged + swallowed so the task survives
/// (mirrors `billing_reconcile` / `spend_reconcile`).
#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(
        tick_secs,
        provider = state.metering_provider.kind().as_str(),
        "control metering_export cron starting"
    );
    loop {
        match tick(&state).await {
            Ok(n) if n > 0 => {
                tracing::info!(exported = n, "control metering_export sweep completed");
            }
            Ok(_) => { /* steady state; nothing new to export */ }
            Err(e) => {
                tracing::error!(error = %e, "control metering_export tick failed");
            }
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// Run one export sweep for the CURRENT calendar-month period, stamping events
/// at the real wall-clock instant. Exposed so an integration test can drive a
/// single deterministic tick. Returns the number of apps that had a non-zero
/// delta pushed this sweep.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    tick_at(state, current_period_start_unix(), Utc::now().timestamp()).await
}

/// Sweep core, parameterized on the period AND the consumption instant `now` so
/// a test can drive an explicit `period_start` and a deterministic event
/// timestamp. `now` (unix seconds) is the instant each meter event is stamped at
/// (C1: NEVER `period.end`, which is a future timestamp Stripe rejects). Takes
/// the advisory lock for the whole sweep (multi-instance safety) and exports
/// each owned app's CURRENT cumulative billable CU as a DELTA reconciled against
/// the external meter's aggregate.
#[allow(clippy::future_not_send)]
pub async fn tick_at(state: &AppState, period_start: i64, now: i64) -> Result<usize, RegistryError> {
    // Multi-instance safety: single-flight the sweep fleet-wide. A loser skips
    // this tick (the `metering_exports` high-water still prevents a double-push,
    // but the lock avoids duplicate external round-trips).
    let lock_conn = state.registry.conn().await?;
    let got = lock_conn
        .query(
            "SELECT pg_try_advisory_lock($1) AS locked",
            &[&EXPORT_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await?;
    let acquired = got.first().is_some_and(|r| r.get::<_, bool>("locked"));
    if !acquired {
        tracing::debug!("metering_export: advisory lock held by another instance — skipping tick");
        return Ok(0);
    }

    let result = sweep(state, period_start, now).await;

    if let Err(e) = lock_conn
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&EXPORT_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await
    {
        tracing::warn!(error = %e, "metering_export: advisory unlock failed (frees on conn drop)");
    }

    result
}

/// The advisory-lock-protected body: for each owned app, derive CU, compute the
/// delta over the high-water, push it through the provider, and advance the
/// high-water on success.
#[allow(clippy::future_not_send)]
async fn sweep(state: &AppState, period_start: i64, now: i64) -> Result<usize, RegistryError> {
    // Creator→apps via ownership (same H1 join the billing sweep uses):
    // app_members WHERE role='owner'. DISTINCT ON (app_id) so a fan-out of owner
    // rows can never export the same app twice. Apps with no owner row are
    // absent here and skipped (no billable creator).
    let conn = state.registry.conn().await?;
    let owner_rows = conn
        .query(
            "SELECT DISTINCT ON (m.app_id) m.user_id AS creator_id, m.app_id \
             FROM zeroship.app_members m \
             WHERE m.role = 'owner' \
             ORDER BY m.app_id, m.user_id",
            &[],
        )
        .await?;
    // Bulk pre-filter (single fleet-wide query): the set of apps that COULD have
    // a non-zero delta this period — those with at least one `usage_aggregates`
    // row for the swept period (gross usage exists) UNION those carrying a
    // non-zero export high-water (a prior export whose C2 self-heal / further
    // delta must still be considered). An app in NEITHER set has gross 0 AND
    // high-water 0, so its per-app delta is unconditionally 0 (`gross ≤ hw`) — a
    // guaranteed no-op. Skipping those here means the per-app path (which opens a
    // fresh, SCRAM-authenticated PG connection per app — there is no pool) runs
    // only O(apps-with-activity), not O(every-app-ever-owned). Without this, a
    // long-lived deployment's sweep cost grows with the TOTAL app count even
    // though almost none have new usage in any given tick.
    let period_ts: DateTime<Utc> = Utc
        .timestamp_opt(period_start, 0)
        .single()
        .ok_or_else(|| RegistryError::Database(format!("invalid period_start {period_start}")))?;
    let active_app_rows = conn
        .query(
            "SELECT app_id FROM zeroship.usage_aggregates WHERE period_start = $1 \
             UNION \
             SELECT app_id FROM zeroship.metering_exports \
               WHERE period_start = $1 AND exported_units > 0",
            &[&period_ts],
        )
        .await?;
    let active_apps: std::collections::HashSet<Uuid> =
        active_app_rows.iter().map(|r| r.get::<_, Uuid>("app_id")).collect();

    // BTreeMap for deterministic creator ordering. Only apps in the active set are
    // retained — the rest are provable no-ops (see the bulk pre-filter above).
    let mut apps_by_creator: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for row in &owner_rows {
        let app_id: Uuid = row.get("app_id");
        if !active_apps.contains(&app_id) {
            continue;
        }
        let creator_id: Uuid = row.get("creator_id");
        apps_by_creator.entry(creator_id).or_default().push(app_id);
    }

    // Load the GLOBAL weight table ONCE per tick — the SAME load the spend cap
    // and the billing reconciler use, so the CU pushed == the CU enforced.
    let pricing = PricingStore::new(state.registry.clone());
    let weights = pricing.weights().await?;
    // The catalog resolves each app's plan so the export can subtract the plan's
    // `included_units` — pushing BILLABLE CU (M1), matching what `charge_cents`
    // (and thus the spend cap) treats as billable. Built ONCE per sweep.
    let catalog = PlanCatalog::new(state.registry.clone());

    let period = BillingPeriod {
        start: period_start,
        end: period_end_unix(period_start),
    };

    let mut exported = 0usize;
    for (creator_id, app_ids) in &apps_by_creator {
        // Resolve the creator's customer ONCE. A creator with no saved customer
        // cannot be metered on the external rail — skip their apps (the local
        // ledger + spend cap are unaffected; this is export-only).
        let customer = match state.stripe_store.get_customer(*creator_id).await {
            Ok(Some(c)) => CustomerRef(c),
            Ok(None) => {
                tracing::debug!(
                    creator_id = %creator_id,
                    "metering_export: creator has no saved customer — skipping export for their apps"
                );
                continue;
            }
            Err(e) => return Err(RegistryError::Database(format!("get_customer: {e}"))),
        };
        let creator_billing = CreatorBilling {
            creator_id: *creator_id,
            email: String::new(),
            customer: Some(customer.clone()),
        };

        for app_id in app_ids {
            match export_app(
                state, &catalog, &weights, &customer, &creator_billing, app_id,
                period, now,
            )
            .await
            {
                Ok(true) => exported += 1,
                Ok(false) => { /* delta 0 or skipped — nothing pushed */ }
                Err(e) => {
                    // A per-app failure must not abort the sweep — log + continue
                    // so one app's external hiccup doesn't starve others. The
                    // high-water was NOT advanced (we only advance on success),
                    // so the next sweep re-pushes the SAME window (same
                    // identifier ⇒ deduped).
                    tracing::error!(
                        app_id = %app_id,
                        creator_id = %creator_id,
                        error = %e,
                        "metering_export: failed to export app usage — continuing"
                    );
                }
            }
        }
    }
    Ok(exported)
}

/// Export ONE app's current-period delta. Returns `Ok(true)` if a non-zero
/// delta was pushed (and the high-water advanced), `Ok(false)` for a no-op
/// (delta 0). On a push failure it records the failure durably (M2) and returns
/// `Err` so the sweep logs + continues.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::future_not_send)]
async fn export_app(
    state: &AppState,
    catalog: &PlanCatalog,
    weights: &crate::pricing::MetricWeights,
    customer: &CustomerRef,
    creator_billing: &CreatorBilling,
    app_id: &Uuid,
    period: BillingPeriod,
    now: i64,
) -> Result<bool, RegistryError> {
    // Open ONE connection for ALL of this app's reads + writes this tick. The
    // control-plane `Registry` opens a fresh PG connection per query (no pool), so
    // a fleet-wide sweep that opened a connection per helper call would pay a TCP
    // + startup handshake several times PER APP — O(apps) handshakes per tick. The
    // sweep is read-heavy and per-app independent, so a single borrowed connection
    // (threaded through `period_totals_on` / `lookup_plan_id_on` / the high-water
    // read + writes) collapses that to one handshake per app.
    let conn = state.registry.conn().await?;

    let period_ts: DateTime<Utc> = Utc
        .timestamp_opt(period.start, 0)
        .single()
        .ok_or_else(|| RegistryError::Database(format!("invalid period_start {}", period.start)))?;

    // 1. CURRENT cumulative GROSS CU — the SAME derivation the spend cap uses.
    let usage = Metering::period_totals_on(&conn, app_id, period.start).await?;
    let gross_units = total_units(weights, &usage).map_err(|e| {
        // An overflow is a hard error (never a clamp) — skip this app's export
        // this tick; the high-water is untouched so a later (fixed) tick retries.
        RegistryError::Database(format!(
            "metering_export: compute-unit overflow for app {app_id}: {e}"
        ))
    })?;

    // 2. The per-(app, period) high-water (cumulative billable CU already pushed).
    let high_water: u64 = conn
        .query(
            "SELECT exported_units FROM zeroship.metering_exports \
             WHERE app_id = $1 AND period_start = $2",
            &[app_id, &period_ts],
        )
        .await?
        .first()
        .map_or(0, |r| r.get::<_, i64>("exported_units").max(0) as u64);

    // 3a. EARLY fast-path no-op (before the plan lookup): `current_units` is
    //     `gross − included ≤ gross`, so if the high-water already covers the
    //     GROSS total it certainly covers the billable `current`. The fleet-wide
    //     sweep visits EVERY owned app each tick, and the vast majority have no new
    //     usage for the swept period (gross == high_water, both often 0). Bailing
    //     here skips BOTH the plan-catalog round-trip AND the external read for
    //     those apps — the dominant cost when the sweep scans many apps.
    if gross_units <= high_water {
        return Ok(false);
    }

    // M1 — BILLABLE CU parity: subtract the plan's `included_units` so the CU
    // pushed to Stripe == the CU `charge_cents` (and thus the spend cap) treats
    // as billable. `billable = max(0, gross − included)`, identical to
    // `ChargeBreakdown.billable_units`. The plan's `base_fee_cents` is a SEPARATE
    // Stripe subscription line — NOT part of the metered usage. An app whose plan
    // is missing from the catalog is skipped (the local ledger is unaffected).
    let included_units = match lookup_plan_id_on(&conn, app_id).await? {
        Some(plan_id) => match catalog.get(&plan_id).await? {
            Some(plan) => plan.price.included_units,
            None => {
                tracing::warn!(
                    app_id = %app_id, plan_id = %plan_id,
                    "metering_export: plan not in catalog — skipping app's export"
                );
                return Ok(false);
            }
        },
        None => 0, // no plan FK ⇒ no included quota
    };
    let current_units = gross_units.saturating_sub(included_units);

    // 3b. Fast-path no-op after the included-units subtraction: the local
    //     high-water already covers the BILLABLE `current` (e.g. all of this
    //     period's gross usage is within the plan's included quota). A re-run over
    //     an unchanged total (or a crash AFTER the high-water write) pushes
    //     nothing and skips the external round-trip.
    if current_units <= high_water {
        return Ok(false);
    }

    // C2 — close the >24h over-bill window. The local high-water is stale by
    // construction if a prior drive crashed AFTER the Stripe push but BEFORE the
    // high-water UPDATE: re-driven past Stripe's ~24h `identifier` dedup window, a
    // blind re-push of `current − high_water` would be SUMMED twice. So reconcile
    // against Stripe's OWN aggregated meter value: `already = max(high_water,
    // stripe_aggregate)`; push only `current − already`. The MAX never
    // double-counts (Stripe's aggregate authoritatively reflects what landed) and
    // never under-counts on Stripe's read lag (the high-water floors it). The
    // guarantee rides on Stripe's aggregate, NOT on the 24h identifier window.
    let stripe_aggregate = match state
        .metering_provider
        .reported_total(state, customer, period)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            record_export_failure(&conn, app_id, &period_ts, &format!("reported_total: {e}")).await;
            return Err(RegistryError::Database(format!(
                "metering_export: reported_total for app {app_id}: {e}"
            )));
        }
    };
    let already = high_water.max(stripe_aggregate);
    let delta = current_units.saturating_sub(already);
    if delta == 0 {
        // Stripe's aggregate already reflects `current` (a crash-then-re-drive
        // where the push landed but the high-water never advanced). No re-push;
        // just advance the local high-water to match + clear any failure state.
        advance_high_water(&conn, app_id, &period_ts, current_units).await?;
        return Ok(false);
    }

    // 4. Deterministic identifier for THIS already→current window, then push the
    //    delta through the provider (Stripe meter_events), stamped at `now` (C1).
    let identifier = export_identifier(app_id, period.start, already, current_units);
    if let Err(e) = state
        .metering_provider
        .report_usage(state, customer, *app_id, period, delta, &identifier, now)
        .await
    {
        // M2 — durable failure surface: a logged-only failure lets a permanently
        // mis-provisioned app under-bill forever invisibly. Record it (bump
        // consecutive_failures, stamp last_error/last_attempt_at) so it is
        // queryable + alertable, then propagate (the sweep logs + continues; the
        // high-water is NOT advanced ⇒ the next sweep retries the SAME window).
        record_export_failure(&conn, app_id, &period_ts, &format!("report_usage: {e}")).await;
        return Err(RegistryError::Database(format!(
            "metering_export: report_usage for app {app_id}: {e}"
        )));
    }
    // `creator_billing` is threaded for parity with the billing sweep's
    // per-creator shape (and so an OpenMeter subject could key on the creator);
    // the Stripe rail keys on the customer, so it is currently unused here.
    let _ = creator_billing;

    // 5. ONLY on a successful push, advance the high-water to `current` and clear
    //    the failure state. A crash BEFORE this write leaves the OLD high-water ⇒
    //    the next sweep reconciles against Stripe's aggregate (which now reflects
    //    this push) ⇒ delta 0 ⇒ no double-count.
    advance_high_water(&conn, app_id, &period_ts, current_units).await?;

    Ok(true)
}

/// Advance the per-(app, period) high-water to `current` and RESET the failure
/// surface (consecutive_failures → 0, last_error → NULL) — the success path.
#[allow(clippy::future_not_send)]
async fn advance_high_water(
    conn: &compio_postgres::Client,
    app_id: &Uuid,
    period_ts: &DateTime<Utc>,
    current_units: u64,
) -> Result<(), RegistryError> {
    let current_i64 = i64::try_from(current_units).map_err(|_| {
        RegistryError::Database(format!(
            "metering_export: current_units {current_units} exceeds i64::MAX — refusing to clamp"
        ))
    })?;
    conn.execute(
        "INSERT INTO zeroship.metering_exports \
             (app_id, period_start, exported_units, updated_at, \
              consecutive_failures, last_error, last_attempt_at) \
         VALUES ($1, $2, $3, NOW(), 0, NULL, NOW()) \
         ON CONFLICT (app_id, period_start) \
         DO UPDATE SET exported_units = EXCLUDED.exported_units, updated_at = NOW(), \
                       consecutive_failures = 0, last_error = NULL, last_attempt_at = NOW()",
        &[app_id, period_ts, &current_i64],
    )
    .await?;
    Ok(())
}

/// M2 — record a durable export failure for `(app, period)`: bump
/// `consecutive_failures`, stamp `last_error` + `last_attempt_at`, WITHOUT
/// touching the `exported_units` high-water (a failed push exported nothing). The
/// row is UPSERTed so a from-the-FIRST-attempt failure (no prior success) is
/// still recorded. Best-effort: a bookkeeping write failure is logged, never
/// allowed to mask the original export error.
#[allow(clippy::future_not_send)]
async fn record_export_failure(
    conn: &compio_postgres::Client,
    app_id: &Uuid,
    period_ts: &DateTime<Utc>,
    error: &str,
) {
    // Cap the stored error so a pathological message can't bloat the row.
    let truncated: String = error.chars().take(1000).collect();
    if let Err(e) = conn
        .execute(
            "INSERT INTO zeroship.metering_exports \
                 (app_id, period_start, exported_units, updated_at, \
                  consecutive_failures, last_error, last_attempt_at) \
             VALUES ($1, $2, 0, NOW(), 1, $3, NOW()) \
             ON CONFLICT (app_id, period_start) \
             DO UPDATE SET consecutive_failures = zeroship.metering_exports.consecutive_failures + 1, \
                           last_error = EXCLUDED.last_error, last_attempt_at = NOW()",
            &[app_id, period_ts, &truncated],
        )
        .await
    {
        tracing::warn!(
            app_id = %app_id, error = %e,
            "metering_export: failed to record export-failure bookkeeping (continuing)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tick_is_hourly() {
        assert_eq!(DEFAULT_TICK_SECS, 3600);
    }

    #[test]
    fn export_identifier_is_deterministic_and_window_keyed() {
        let app = Uuid::from_u128(0xAB);
        let p = 1_700_000_000i64;
        // Stable for a fixed window (a re-push of the same window dedups).
        assert_eq!(
            export_identifier(&app, p, 0, 100),
            export_identifier(&app, p, 0, 100),
        );
        // A NEW window (usage grew → different `current`) yields a NEW id.
        assert_ne!(
            export_identifier(&app, p, 0, 100),
            export_identifier(&app, p, 100, 250),
        );
        // Distinct per app + per period.
        assert_ne!(
            export_identifier(&app, p, 0, 100),
            export_identifier(&Uuid::from_u128(0xCD), p, 0, 100),
        );
        assert_ne!(
            export_identifier(&app, p, 0, 100),
            export_identifier(&app, p + 1, 0, 100),
        );
    }

    #[test]
    fn advisory_key_is_distinct_from_billing_and_spend() {
        // The three sweeps must use DISTINCT advisory keys or they would
        // serialize against each other fleet-wide.
        assert_ne!(EXPORT_SWEEP_ADVISORY_LOCK_KEY, 0x7a73_6269_6c6c_0001); // billing
        assert_ne!(EXPORT_SWEEP_ADVISORY_LOCK_KEY, 0x7a73_7370_6e64_0001); // spend
    }
}
