//! Metering-export cron (M-Stripe, blueprint §M5/§M9): the periodic sweep that
//! forwards each creator's CURRENT-period compute units to an external meter
//! (Stripe Billing Meters / OpenMeter). Spawned ONLY for an export provider —
//! NEVER for native (where `report_usage` is a no-op and spawning it would be
//! pure waste, blueprint §M5 table).
//!
//! ## The grain — PER CREATOR / per customer (HIGH-severity fix)
//!
//! The external meter rails this cron feeds aggregate PER CUSTOMER, not per app:
//! Stripe Billing Meters sums `event_summaries?customer=…`; OpenMeter sums per
//! `subject` (the creator handle). An earlier per-APP design pushed each app's
//! delta but reconciled it against that per-CUSTOMER aggregate — so once a
//! creator's FIRST app pushed its CU the customer aggregate covered it, and EVERY
//! subsequent app of the same creator computed `delta = 0` (its CU never billed,
//! its high-water silently advanced to mask the gap). Every metered app after the
//! first under-billed. The fix reconciles + pushes at the grain the meter actually
//! aggregates: PER CREATOR.
//!
//! ## The crux — idempotent DELTA export
//!
//! Stripe meter events are SUMMED on Stripe's side, so the cron must push the
//! CU CONSUMED SINCE THE LAST EXPORT (the delta), not the cumulative total, and
//! must never double-push on a cron re-run / crash. Per CREATOR per tick:
//!
//!   1. Derive the creator's CURRENT cumulative BILLABLE CU by summing, over ALL
//!      the creator's apps, the PER-APP floored billable
//!      `pricing::billable_units(plan.price, period_totals(app, period_start),
//!      weights)` = `max(0, total_units − plan.included_units)` — the EXACT
//!      per-app quantity `charge_cents` bills (spend cap + invoicing), re-derived
//!      from the raw, re-weightable `usage_aggregates` (NO CU column is added).
//!      The included subtraction is floored PER APP (plans differ per app), THEN
//!      summed ⇒ `current_creator = Σ_app max(0, gross_app − included_app)`.
//!      Subtracting a single creator-level included quota would let one app's
//!      unused quota offset another app's overage (under-bills, disagrees with
//!      the spend cap).
//!   2. Read the per-`(creator, period)` high-water `exported_units` from
//!      `metering_exports` (0057). `delta = current_creator − exported`.
//!   3. `delta == 0` ⇒ nothing new ⇒ SKIP (the no-op that makes a re-run/crash
//!      safe — a second sweep over an unchanged total pushes nothing).
//!   4. Push the delta via `provider.report_usage(customer, …, period, delta,
//!      identifier)`. The `identifier` is DETERMINISTIC from
//!      `(creator, period, exported→current window)` so a Stripe-side replay of
//!      the SAME window dedups too (defense in depth).
//!   5. ONLY on a successful push, UPSERT `exported_units = current_creator`. A
//!      crash between the push and this write leaves the OLD high-water, so the
//!      next sweep re-pushes the SAME window — same `identifier` ⇒ Stripe dedups ⇒
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

use chrono::Utc;
use uuid::Uuid;

use crate::cron::billing_reconcile::{lookup_plan_id_on, period_end_unix};
use crate::metering::{current_period_start_unix, period_date, Metering};
use crate::plan_catalog::PlanCatalog;
use crate::pricing::{billable_units, total_units};
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

/// Deterministic dedup `identifier` for ONE meter push: the `(creator, period,
/// exported→current window)` tuple. Stable for a fixed window so a re-push of
/// the SAME window (a crash before the high-water write) replays the SAME
/// identifier and Stripe dedups it. A NEW window (usage grew → a different
/// `current`) yields a NEW identifier so its delta is counted once. Keyed on the
/// CREATOR (the external meter's per-customer aggregation grain), NOT a single
/// app — one creator pushes one delta per period.
#[must_use]
pub fn export_identifier(
    creator_id: &Uuid,
    period_start_unix: i64,
    exported_units: u64,
    current_units: u64,
) -> String {
    format!("export:{creator_id}:{period_start_unix}:{exported_units}:{current_units}")
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
/// each creator's CURRENT cumulative billable CU (summed across their apps) as a
/// DELTA reconciled against the external meter's per-customer aggregate.
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

/// The advisory-lock-protected body: for each creator, sum BILLABLE CU across
/// their apps, compute the delta over the creator's high-water, push it through
/// the provider, and advance the high-water on success.
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
    let period_d = period_date(period_start);
    // Bulk pre-filter A (single fleet-wide query): the set of apps with at least
    // one `usage_aggregates` row for the swept period (gross usage exists). An app
    // not here has gross 0, so it contributes 0 to its creator's gross.
    let active_app_rows = conn
        .query(
            "SELECT DISTINCT app_id FROM zeroship.usage_aggregates WHERE period = $1::date",
            &[&period_d],
        )
        .await?;
    let active_apps: std::collections::HashSet<Uuid> =
        active_app_rows.iter().map(|r| r.get::<_, Uuid>("app_id")).collect();
    // Bulk pre-filter B: creators carrying a NON-ZERO export high-water for the
    // period. Even if all their apps dropped to zero gross this tick, such a
    // creator must still be VISITED so the C2 self-heal / aggregate reconcile can
    // run (the high-water is creator-keyed now). A creator with neither active
    // apps NOR a high-water is a provable no-op (gross 0 AND hw 0 ⇒ delta 0) and
    // is skipped — keeping the sweep O(creators-with-activity), not
    // O(every-creator-ever-owning-an-app); the per-creator path opens a fresh,
    // SCRAM-authenticated PG connection (there is no pool).
    let hw_creator_rows = conn
        .query(
            "SELECT creator_id FROM zeroship.metering_exports \
             WHERE period = $1::date AND exported_units > 0",
            &[&period_d],
        )
        .await?;
    let hw_creators: std::collections::HashSet<Uuid> =
        hw_creator_rows.iter().map(|r| r.get::<_, Uuid>("creator_id")).collect();

    // Group EVERY owned app under its creator (BTreeMap for deterministic creator
    // ordering). The per-creator export then sums BILLABLE CU across the apps; an
    // app with no usage this period simply contributes 0. A creator is retained
    // iff at least one of their apps has activity OR they carry a non-zero
    // high-water (the pre-filters above) — everyone else is a provable no-op.
    let mut all_apps_by_creator: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for row in &owner_rows {
        let creator_id: Uuid = row.get("creator_id");
        let app_id: Uuid = row.get("app_id");
        all_apps_by_creator.entry(creator_id).or_default().push(app_id);
    }
    let apps_by_creator: BTreeMap<Uuid, Vec<Uuid>> = all_apps_by_creator
        .into_iter()
        .filter(|(creator_id, app_ids)| {
            hw_creators.contains(creator_id) || app_ids.iter().any(|a| active_apps.contains(a))
        })
        .collect();

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
        // cannot be metered on the external rail — skip them (the local ledger +
        // spend cap are unaffected; this is export-only).
        let customer = match state.stripe_store.get_customer(*creator_id).await {
            Ok(Some(c)) => CustomerRef(c),
            Ok(None) => {
                tracing::debug!(
                    creator_id = %creator_id,
                    "metering_export: creator has no saved customer — skipping export"
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

        match export_creator(
            state, &catalog, &weights, &customer, &creator_billing, app_ids,
            period, now,
        )
        .await
        {
            Ok(true) => exported += 1,
            Ok(false) => { /* delta 0 or skipped — nothing pushed */ }
            Err(e) => {
                // A per-creator failure must not abort the sweep — log + continue
                // so one creator's external hiccup doesn't starve others. The
                // high-water was NOT advanced (we only advance on success), so the
                // next sweep re-pushes the SAME window (same identifier ⇒ deduped).
                tracing::error!(
                    creator_id = %creator_id,
                    error = %e,
                    "metering_export: failed to export creator usage — continuing"
                );
            }
        }
    }
    Ok(exported)
}

/// Export ONE creator's current-period delta — the BILLABLE CU summed across ALL
/// their apps, reconciled against the external meter's per-CUSTOMER aggregate and
/// pushed as ONE customer-level delta. Returns `Ok(true)` if a non-zero delta was
/// pushed (and the high-water advanced), `Ok(false)` for a no-op (delta 0). On a
/// push failure it records the failure durably (M2) and returns `Err` so the
/// sweep logs + continues.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::future_not_send)]
async fn export_creator(
    state: &AppState,
    catalog: &PlanCatalog,
    weights: &crate::pricing::MetricWeights,
    customer: &CustomerRef,
    creator_billing: &CreatorBilling,
    app_ids: &[Uuid],
    period: BillingPeriod,
    now: i64,
) -> Result<bool, RegistryError> {
    let creator_id = creator_billing.creator_id;
    // Open ONE connection for ALL of this creator's reads + writes this tick. The
    // control-plane `Registry` opens a fresh PG connection per query (no pool), so
    // a fleet-wide sweep that opened a connection per helper call would pay a TCP
    // + startup handshake several times PER CREATOR. The sweep is read-heavy and
    // per-creator independent, so a single borrowed connection (threaded through
    // `period_totals_on` / `lookup_plan_id_on` / the high-water read + writes)
    // collapses that to one handshake per creator.
    let conn = state.registry.conn().await?;

    let period_d = period_date(period.start);

    // 1. CURRENT cumulative BILLABLE CU summed ACROSS ALL the creator's apps.
    //    The included subtraction is PER APP (`plan_id` is per-app; apps under one
    //    creator can carry DIFFERENT plans with DIFFERENT included quotas), so we
    //    floor `max(0, gross − included)` for EACH app via the SAME authoritative
    //    `pricing::billable_units` the spend cap + invoicing use (`charge_cents`),
    //    THEN sum. Summing gross and subtracting one creator-level included quota
    //    (the prior, broken form) lets one app's unused quota offset another app's
    //    overage — under-billing and disagreeing with the spend cap. Per-app floor
    //    then sum cannot.
    //
    //    This is also the core of the HIGH-severity grain fix: a per-app delta
    //    reconciled against a per-customer aggregate silently zeroed every app
    //    after the first; the creator-level SUM of per-app billable cannot.
    let mut current_units: u64 = 0;
    for app_id in app_ids {
        let usage = Metering::period_totals_on(&conn, app_id, period.start).await?;

        // The app's plan resolves its `included_units`. An app with no plan FK
        // has no included quota (billable == gross); an app whose plan is missing
        // from the catalog is SKIPPED entirely (contributes 0) so it neither
        // over- nor under-counts. `billable_units(price, usage, weights)` is the
        // EXACT per-app floored definition `charge_cents` bills, so the CU pushed
        // == the CU the spend cap enforces == the CU invoicing charges.
        let app_billable = match lookup_plan_id_on(&conn, app_id).await? {
            Some(plan_id) => match catalog.get(&plan_id).await? {
                Some(plan) => billable_units(&plan.price, &usage, weights),
                None => {
                    tracing::warn!(
                        app_id = %app_id, plan_id = %plan_id,
                        "metering_export: plan not in catalog — excluding app from the creator's export"
                    );
                    continue;
                }
            },
            // No plan FK ⇒ no included quota ⇒ billable == gross. Mirror
            // `billable_units` with an empty (zero-included) price would require a
            // synthetic plan; instead derive gross directly via `total_units`.
            None => total_units(weights, &usage),
        }
        .map_err(|e| {
            // An overflow is a hard error (never a clamp) — skip this creator's
            // export this tick; the high-water is untouched so a later (fixed) tick
            // retries.
            RegistryError::Database(format!(
                "metering_export: compute-unit overflow for app {app_id}: {e}"
            ))
        })?;

        current_units = current_units.saturating_add(app_billable);
    }

    // 2. The per-(creator, period) high-water (cumulative billable CU already
    //    pushed for this customer).
    let high_water: u64 = conn
        .query(
            "SELECT exported_units FROM zeroship.metering_exports \
             WHERE creator_id = $1 AND period = $2::date",
            &[&creator_id, &period_d],
        )
        .await?
        .first()
        .map_or(0, |r| r.get::<_, i64>("exported_units").max(0) as u64);

    // 3. Fast-path no-op: the local high-water already covers the BILLABLE
    //    `current` (e.g. no new usage since the last export, or all of this
    //    period's gross is within the creators' included quota). A re-run over an
    //    unchanged total (or a crash AFTER the high-water write) pushes nothing and
    //    skips the external round-trip.
    if current_units <= high_water {
        return Ok(false);
    }

    // C2 — close the >24h over-bill window. The local high-water is stale by
    // construction if a prior drive crashed AFTER the Stripe push but BEFORE the
    // high-water UPDATE: re-driven past Stripe's ~24h `identifier` dedup window, a
    // blind re-push of `current − high_water` would be SUMMED twice. So reconcile
    // against the meter's OWN aggregated value FOR THIS CUSTOMER: `already =
    // max(high_water, customer_aggregate)`; push only `current − already`. The MAX
    // never double-counts (the aggregate authoritatively reflects what landed) and
    // never under-counts on read lag (the high-water floors it). The guarantee
    // rides on the customer aggregate, NOT on the 24h identifier window.
    let customer_aggregate = match state
        .metering_provider
        .reported_total(state, customer, period)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            record_export_failure(&conn, &creator_id, &period_d, &format!("reported_total: {e}")).await;
            return Err(RegistryError::Database(format!(
                "metering_export: reported_total for creator {creator_id}: {e}"
            )));
        }
    };
    let already = high_water.max(customer_aggregate);
    let delta = current_units.saturating_sub(already);
    if delta == 0 {
        // The aggregate already reflects `current` (a crash-then-re-drive where the
        // push landed but the high-water never advanced). No re-push; just advance
        // the local high-water to match + clear any failure state.
        advance_high_water(&conn, &creator_id, &period_d, current_units).await?;
        return Ok(false);
    }

    // 4. Deterministic identifier for THIS already→current window, then push the
    //    delta through the provider (Stripe meter_events / OpenMeter CloudEvent),
    //    stamped at `now` (C1). The push is per CUSTOMER — one delta per creator.
    let identifier = export_identifier(&creator_id, period.start, already, current_units);
    if let Err(e) = state
        .metering_provider
        .report_usage(state, customer, period, delta, &identifier, now)
        .await
    {
        // M2 — durable failure surface: a logged-only failure lets a permanently
        // mis-provisioned creator under-bill forever invisibly. Record it (bump
        // consecutive_failures, stamp last_error/last_attempt_at) so it is
        // queryable + alertable, then propagate (the sweep logs + continues; the
        // high-water is NOT advanced ⇒ the next sweep retries the SAME window).
        record_export_failure(&conn, &creator_id, &period_d, &format!("report_usage: {e}")).await;
        return Err(RegistryError::Database(format!(
            "metering_export: report_usage for creator {creator_id}: {e}"
        )));
    }

    // 5. ONLY on a successful push, advance the high-water to `current` and clear
    //    the failure state. A crash BEFORE this write leaves the OLD high-water ⇒
    //    the next sweep reconciles against the customer aggregate (which now
    //    reflects this push) ⇒ delta 0 ⇒ no double-count.
    advance_high_water(&conn, &creator_id, &period_d, current_units).await?;

    Ok(true)
}

/// Advance the per-(creator, period) high-water to `current` and RESET the
/// failure surface (consecutive_failures → 0, last_error → NULL) — the success
/// path.
#[allow(clippy::future_not_send)]
async fn advance_high_water(
    conn: &compio_postgres::Client,
    creator_id: &Uuid,
    period: &chrono::NaiveDate,
    current_units: u64,
) -> Result<(), RegistryError> {
    let current_i64 = i64::try_from(current_units).map_err(|_| {
        RegistryError::Database(format!(
            "metering_export: current_units {current_units} exceeds i64::MAX — refusing to clamp"
        ))
    })?;
    conn.execute(
        "INSERT INTO zeroship.metering_exports \
             (creator_id, period, exported_units, updated_at, \
              consecutive_failures, last_error, last_attempt_at) \
         VALUES ($1, $2::date, $3, NOW(), 0, NULL, NOW()) \
         ON CONFLICT (creator_id, period) \
         DO UPDATE SET exported_units = EXCLUDED.exported_units, updated_at = NOW(), \
                       consecutive_failures = 0, last_error = NULL, last_attempt_at = NOW()",
        &[creator_id, period, &current_i64],
    )
    .await?;
    Ok(())
}

/// M2 — record a durable export failure for `(creator, period)`: bump
/// `consecutive_failures`, stamp `last_error` + `last_attempt_at`, WITHOUT
/// touching the `exported_units` high-water (a failed push exported nothing). The
/// row is UPSERTed so a from-the-FIRST-attempt failure (no prior success) is
/// still recorded. Best-effort: a bookkeeping write failure is logged, never
/// allowed to mask the original export error.
#[allow(clippy::future_not_send)]
async fn record_export_failure(
    conn: &compio_postgres::Client,
    creator_id: &Uuid,
    period: &chrono::NaiveDate,
    error: &str,
) {
    // Cap the stored error so a pathological message can't bloat the row.
    let truncated: String = error.chars().take(1000).collect();
    if let Err(e) = conn
        .execute(
            "INSERT INTO zeroship.metering_exports \
                 (creator_id, period, exported_units, updated_at, \
                  consecutive_failures, last_error, last_attempt_at) \
             VALUES ($1, $2::date, 0, NOW(), 1, $3, NOW()) \
             ON CONFLICT (creator_id, period) \
             DO UPDATE SET consecutive_failures = zeroship.metering_exports.consecutive_failures + 1, \
                           last_error = EXCLUDED.last_error, last_attempt_at = NOW()",
            &[creator_id, period, &truncated],
        )
        .await
    {
        tracing::warn!(
            creator_id = %creator_id, error = %e,
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
        let creator = Uuid::from_u128(0xAB);
        let p = 1_700_000_000i64;
        // Stable for a fixed window (a re-push of the same window dedups).
        assert_eq!(
            export_identifier(&creator, p, 0, 100),
            export_identifier(&creator, p, 0, 100),
        );
        // A NEW window (usage grew → different `current`) yields a NEW id.
        assert_ne!(
            export_identifier(&creator, p, 0, 100),
            export_identifier(&creator, p, 100, 250),
        );
        // Distinct per creator + per period.
        assert_ne!(
            export_identifier(&creator, p, 0, 100),
            export_identifier(&Uuid::from_u128(0xCD), p, 0, 100),
        );
        assert_ne!(
            export_identifier(&creator, p, 0, 100),
            export_identifier(&creator, p + 1, 0, 100),
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
