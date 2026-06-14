//! Full usage-segment proration (billing-ops gap #26, PR-4; design decision 1
//! OVERRIDE). Two halves:
//!
//! 1. **The write side** ([`record_plan_change`]) — invoked by `api.rs::set_plan`.
//!    On every plan change it appends an append-only `plan_change_events` row that
//!    snapshots BOTH the plan base fees (server-derived from the catalog, NEVER
//!    client-supplied — CRITICAL-4) AND the app's cumulative `usage_aggregates`
//!    totals (`usage_at_change` JSONB) read in the SAME txn as the `apps.plan_id`
//!    flip. Per-period change cap ([`MAX_PLAN_CHANGES_PER_PERIOD`]); past the cap
//!    the plan still flips (the creator IS on the new plan) but NO snapshot is
//!    recorded, so the tail prices under the actually-running plan (MAJOR-4). The
//!    whole write takes the per-creator advisory lock so it cannot interleave with
//!    a month-end reconcile (MISSING-4), and a change whose effective period is
//!    already finalized is attributed to the NEXT period.
//!
//! 2. **The read/price side** ([`build_segments`]) — invoked by `bill_creator`.
//!    An app with N change events in the period splits into N+1 segments (ordered
//!    by `effective_at`). Each segment's usage is the cumulative DELTA between
//!    consecutive snapshots, floored at `max(0, …)` (MAJOR-3). Day-spans are a
//!    half-open calendar-day partition that telescopes to `days_in_period`
//!    exactly (CRITICAL-2). Base fee + included_units are day-weighted; the
//!    base-fee remainder cent lands on the last segment. Zero-day segments merge
//!    into a neighbour (MINOR-6). N=0 degenerates to exactly one `segment_no=0`
//!    segment — byte-for-byte today's single-line behaviour.
//!
//! Zero tokio: all DB work is `compio-postgres` on the caller's connection/txn.

use std::collections::HashMap;

use chrono::{Datelike, TimeZone, Utc};
use compio_postgres::GenericClient;
use uuid::Uuid;

use crate::pricing::PlanPrice;
use crate::registry::{Registry, RegistryError};

/// Per-period plan-change cap (design decision; default 8). Past the cap a
/// `set_plan` still flips `apps.plan_id` but records NO new `plan_change_events`
/// snapshot — so a creator cannot manufacture an unbounded number of favourable
/// micro-segments, and the over-cap tail merges into the final segment priced
/// under the actually-running plan (MAJOR-4: never under a cheaper recorded plan).
pub const MAX_PLAN_CHANGES_PER_PERIOD: i64 = 8;

/// Outcome of [`record_plan_change`], surfaced for tests + observability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanChangeOutcome {
    /// The plan flipped AND a `plan_change_events` snapshot was recorded.
    Recorded {
        /// The `pce_…` id of the appended event.
        event_id: String,
        /// The period the change was attributed to (first-of-month DATE, ISO).
        period: chrono::NaiveDate,
    },
    /// The plan flipped but NO snapshot was recorded because the per-period cap
    /// was already hit. The tail prices under the actually-running plan.
    FlippedNoSnapshotCapHit,
    /// The app row does not exist.
    AppNotFound,
}

/// First-of-month `billing_period` DATE for a unix-seconds instant.
#[must_use]
pub fn period_date_of(unix_secs: i64) -> chrono::NaiveDate {
    let dt = Utc.timestamp_opt(unix_secs, 0).single().unwrap_or_else(Utc::now);
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1)
        .unwrap_or_else(|| dt.date_naive().with_day(1).unwrap_or(dt.date_naive()))
}

/// The first-of-NEXT-month DATE after the month containing `period`. Used to
/// attribute a change whose effective period is already finalized to the next
/// period (MISSING-4).
#[must_use]
pub fn next_period_date(period: chrono::NaiveDate) -> chrono::NaiveDate {
    let (y, m) = if period.month() == 12 {
        (period.year() + 1, 1)
    } else {
        (period.year(), period.month() + 1)
    };
    chrono::NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(period)
}

/// Record a plan change on a transaction the caller owns (so the snapshot read,
/// the `apps.plan_id` flip, and the `plan_change_events` INSERT all commit
/// together). Takes the per-creator advisory lock FIRST so it serializes against
/// a month-end reconcile for the same creator.
///
/// `from_base_fee_cents` / `to_base_fee_cents` are passed by the caller already
/// resolved from the plan CATALOG (server-side) — this function never reads them
/// from any client input. `now_unix` is the effective instant.
///
/// # Errors
/// Propagates DB errors. Returns [`PlanChangeOutcome::AppNotFound`] (not an error)
/// when the app row is absent.
#[allow(clippy::too_many_arguments)]
pub async fn record_plan_change<C: GenericClient + Sync>(
    tx: &C,
    app_id: &Uuid,
    creator_id: &Uuid,
    from_plan_id: Option<&str>,
    to_plan_id: &str,
    from_base_fee_cents: Option<i64>,
    to_base_fee_cents: i64,
    now_unix: i64,
) -> Result<PlanChangeOutcome, RegistryError> {
    // SERIALIZE against the month-end reconcile for THIS creator. The reconcile
    // holds the same per-creator advisory lock for its whole build+finalize, so a
    // plan change cannot interleave INSIDE a finalize — it lands fully before or
    // fully after. `hashtext` (int4) → bigint for pg_advisory_xact_lock(bigint);
    // the lock auto-releases at the caller's commit/rollback. Identical keying to
    // `credit::consume_at_finalize`, so the two contend on the SAME lock.
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtext($1::text)::bigint)",
        &[&creator_id.to_string()],
    )
    .await
    .map_err(|e| RegistryError::Database(e.to_string()))?;

    // Flip the plan (guarded on a real, non-archived plan exactly as
    // `registry::set_plan`). Zero rows ⇒ the app row is gone (the plan validity is
    // checked by the caller before this is reached).
    let flipped = tx
        .execute(
            "UPDATE zeroship.apps SET plan_id = $1, updated_at = NOW() \
             WHERE id = $2 \
               AND EXISTS (SELECT 1 FROM zeroship.plans WHERE id = $1 AND NOT archived)",
            &[&to_plan_id, app_id],
        )
        .await?;
    if flipped == 0 {
        return Ok(PlanChangeOutcome::AppNotFound);
    }

    // Attribute the change to the effective period — UNLESS that period's invoice
    // is already finalized, in which case the proration effect lands in the NEXT
    // period (the flip itself already applied above; only the billing attribution
    // respects the period-finalized boundary). MISSING-4.
    let mut period = period_date_of(now_unix);
    let finalized = tx
        .query(
            "SELECT 1 FROM zeroship.invoices \
             WHERE creator_id = $1 AND period = $2::date AND status = 'finalized'",
            &[creator_id, &period],
        )
        .await?;
    if !finalized.is_empty() {
        period = next_period_date(period);
    }

    // Per-period change cap. Past the cap: the plan flipped (above) but record NO
    // new snapshot — the tail prices under the actually-running plan (MAJOR-4).
    let count_rows = tx
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.plan_change_events \
             WHERE app_id = $1 AND period = $2::date",
            &[app_id, &period],
        )
        .await?;
    let existing: i64 = count_rows.first().map_or(0, |r| r.get::<_, i64>("n"));
    if existing >= MAX_PLAN_CHANGES_PER_PERIOD {
        return Ok(PlanChangeOutcome::FlippedNoSnapshotCapHit);
    }

    // Snapshot the app's cumulative usage_aggregates totals for the ATTRIBUTED
    // period IN THIS TXN (so it reflects exactly the running total at the instant
    // the plan switched, frozen, un-racy). Server-derived, never client-supplied.
    let usage_rows = tx
        .query(
            "SELECT metric, total FROM zeroship.usage_aggregates \
             WHERE app_id = $1 AND period = $2::date",
            &[app_id, &period],
        )
        .await?;
    let mut usage_at_change = serde_json::Map::new();
    for r in &usage_rows {
        usage_at_change.insert(
            r.get::<_, String>("metric"),
            serde_json::Value::from(r.get::<_, i64>("total")),
        );
    }
    let usage_json = serde_json::Value::Object(usage_at_change);

    let event_id = zeroship_core::typed_id::new_plan_change_event_id();
    let effective_at = Utc.timestamp_opt(now_unix, 0).single().unwrap_or_else(Utc::now);
    tx.execute(
        "INSERT INTO zeroship.plan_change_events \
           (id, app_id, period, from_plan_id, to_plan_id, effective_at, \
            from_base_fee_cents, to_base_fee_cents, usage_at_change) \
         VALUES ($1, $2, $3::date, $4, $5, $6, $7, $8, $9)",
        &[
            &event_id,
            app_id,
            &period,
            &from_plan_id,
            &to_plan_id,
            &effective_at,
            &from_base_fee_cents,
            &to_base_fee_cents,
            &usage_json,
        ],
    )
    .await?;

    Ok(PlanChangeOutcome::Recorded { event_id, period })
}

/// Convenience wrapper that opens a connection + transaction on `registry` and
/// runs [`record_plan_change`] inside it (committing on success). This is the
/// single shared path BOTH `api.rs::set_plan` and the proration tests drive, so
/// the tests exercise the REAL server-side write — advisory lock, server-side
/// usage snapshot, plan flip, cap, period-finalized attribution — with no shim.
/// Base fees are supplied by the caller already resolved from the plan catalog.
///
/// # Errors
/// Propagates DB / transaction errors.
#[allow(clippy::too_many_arguments)]
pub async fn record_plan_change_tx(
    registry: &Registry,
    app_id: &Uuid,
    creator_id: &Uuid,
    from_plan_id: Option<&str>,
    to_plan_id: &str,
    from_base_fee_cents: Option<i64>,
    to_base_fee_cents: i64,
    now_unix: i64,
) -> Result<PlanChangeOutcome, RegistryError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    let outcome = record_plan_change(
        &tx,
        app_id,
        creator_id,
        from_plan_id,
        to_plan_id,
        from_base_fee_cents,
        to_base_fee_cents,
        now_unix,
    )
    .await?;
    if matches!(outcome, PlanChangeOutcome::AppNotFound) {
        drop(tx);
    } else {
        tx.commit().await.map_err(|e| RegistryError::Database(e.to_string()))?;
    }
    Ok(outcome)
}

// ===========================================================================
// The read/price side — segment construction for the reconcile loop.
// ===========================================================================

/// A `plan_change_events` row as read for segment construction (effective_at
/// order). `usage_at_change` is the cumulative snapshot at the change instant.
#[derive(Debug, Clone)]
pub struct PlanChange {
    pub to_plan_id: String,
    pub effective_at: chrono::DateTime<Utc>,
    pub usage_at_change: HashMap<String, i64>,
}

/// One priced plan segment of an app's period. The reconcile loop turns each into
/// a frozen `invoice_lines` row (and one Stripe item).
#[derive(Debug, Clone)]
pub struct BilledSegment {
    pub segment_no: i16,
    /// The segment's plan (NOT NULL on the line).
    pub plan_id: String,
    /// `max(0, end − start)` per metric (MAJOR-3 floor) — the segment's metered slice.
    pub usage_delta: HashMap<String, i64>,
    /// `plan.included_units × segment_days / days_in_period`.
    pub included_units: u64,
    /// `plan.base_fee_cents × segment_days / days_in_period` (+remainder on last).
    pub base_fee_cents: u64,
    /// The segment plan's FX (the per-plan override lever). `None` when the plan
    /// inherits AND the global default is unresolved — propagated so the pricer
    /// hits `PricingError::UnresolvedFx` and the sweep fails closed (never $0).
    pub fx_pico_cents_per_unit: Option<u64>,
    /// Half-open day-span `[start_day, end_day)` for the description + audit.
    pub start_day: u32,
    pub end_day: u32,
}

/// Calendar days in the month of `period_start_unix` (28/29/30/31).
#[must_use]
pub fn days_in_period(period_start_unix: i64) -> u32 {
    let dt = Utc.timestamp_opt(period_start_unix, 0).single().unwrap_or_else(Utc::now);
    let (ny, nm) = if dt.month() == 12 { (dt.year() + 1, 1) } else { (dt.year(), dt.month() + 1) };
    let first_next = chrono::NaiveDate::from_ymd_opt(ny, nm, 1).unwrap();
    let first_this = chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).unwrap();
    (first_next - first_this).num_days() as u32
}

/// The 1-based calendar day-of-month of `effective_at` clamped into the period
/// `[1, days_in_period]`. The half-open partition's boundary marker:
/// `segment_days = next_boundary − this_boundary` where boundaries are these
/// day-of-month values (period_start = day 1, period_end = days_in_period + 1).
fn change_day_of_month(effective_at: chrono::DateTime<Utc>, period_start_unix: i64) -> u32 {
    let period_start =
        Utc.timestamp_opt(period_start_unix, 0).single().unwrap_or_else(Utc::now);
    // Same month as the period? If the change predates the period (an event
    // attributed forward from a finalized period would not, but be defensive) it
    // owns from day 1; if it postdates it, it owns from the last day.
    let dim = days_in_period(period_start_unix);
    if effective_at < period_start {
        return 1;
    }
    let day = effective_at.day();
    day.clamp(1, dim)
}

/// `round_half_up(value × num / den)` in u128, saturating into u64. den must be > 0.
fn day_weight(value: u64, num: u64, den: u64) -> u64 {
    if den == 0 {
        return 0;
    }
    let n = u128::from(value) * u128::from(num);
    let rounded = (n + u128::from(den) / 2) / u128::from(den);
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

/// The boundary-finalize step shared by both entry points: given ordered
/// boundaries (plan + start snapshot + start day), compute end snapshots (next
/// boundary's start, or period-end for the last), day-spans, deltas, prices, and
/// merge zero-day segments.
fn finalize_boundaries(
    boundaries: Vec<BoundaryRow>,
    dim: u32,
    period_end_totals: &HashMap<String, i64>,
    plan_prices: &HashMap<String, PlanPrice>,
) -> Vec<BilledSegment> {
    // end_day of segment k = start_day of segment k+1; last = dim+1 (period_end).
    let n = boundaries.len();
    let mut spans: Vec<(u32, u32)> = Vec::with_capacity(n);
    for k in 0..n {
        let start = boundaries[k].start_day;
        let end = if k + 1 < n { boundaries[k + 1].start_day } else { dim + 1 };
        spans.push((start, end));
    }

    // ----- Merge zero-day segments (MINOR-6): fold a span==0 segment's START
    // snapshot away so its usage delta accrues to the surviving neighbour. A
    // zero-day segment has start_day == next start_day. We drop the boundary; the
    // neighbour that keeps its (earlier) start covers the merged delta. Fold into
    // the NEXT segment normally; if it is the LAST, fold into the PREVIOUS.
    let mut keep: Vec<usize> = Vec::with_capacity(n);
    for k in 0..n {
        let (s, e) = spans[k];
        if e > s {
            keep.push(k);
        }
        // zero-day (e == s): dropped — its boundary collapses. Because end-snapshot
        // is the NEXT kept boundary's start, the surviving neighbour's delta spans
        // the dropped segment's usage automatically (telescoping is preserved).
    }
    if keep.is_empty() {
        // Degenerate (every segment zero-day — only possible if dim==0, impossible
        // for a real month). Keep the first to avoid emitting nothing.
        keep.push(0);
    }

    // Recompute spans over the kept boundaries (start = kept boundary start day,
    // end = next kept boundary start day, last = dim+1).
    let kept_n = keep.len();
    let mut segments: Vec<BilledSegment> = Vec::with_capacity(kept_n);
    // First pass: compute integer day-weighted base fees, tracking the rounded sum
    // so the remainder cent can land on the last segment (CRITICAL-2).
    for (seg_idx, &b) in keep.iter().enumerate() {
        let start_day = boundaries[b].start_day;
        let end_day = if seg_idx + 1 < kept_n {
            boundaries[keep[seg_idx + 1]].start_day
        } else {
            dim + 1
        };
        let segment_days = end_day - start_day;

        // END cumulative snapshot = the next KEPT boundary's start snapshot, or the
        // period-end totals for the last segment.
        let end_snapshot: &HashMap<String, i64> = if seg_idx + 1 < kept_n {
            &boundaries[keep[seg_idx + 1]].start_snapshot
        } else {
            period_end_totals
        };
        let start_snapshot = &boundaries[b].start_snapshot;

        // Segment usage delta = max(0, end − start) per metric, over the UNION of
        // the metrics in either snapshot (MAJOR-3 floor: an END-missing metric reads
        // end=0 and pins to 0, never a negative credit).
        let mut usage_delta: HashMap<String, i64> = HashMap::new();
        let metrics: std::collections::HashSet<&String> =
            start_snapshot.keys().chain(end_snapshot.keys()).collect();
        for m in metrics {
            let start = *start_snapshot.get(m).unwrap_or(&0);
            let end = *end_snapshot.get(m).unwrap_or(&0);
            let delta = (end - start).max(0);
            if delta > 0 {
                usage_delta.insert(m.clone(), delta);
            }
        }

        let price = plan_prices.get(&boundaries[b].plan_id).cloned().unwrap_or_default();
        let base_full = price.base_fee_cents;
        let included_full = price.included_units;
        let fx = price.fx_pico_cents_per_unit;

        let included_units = day_weight(included_full, u64::from(segment_days), u64::from(dim));
        // Base fee day-weighted; remainder reconciled on the LAST kept segment.
        let base_fee_cents = day_weight(base_full, u64::from(segment_days), u64::from(dim));

        segments.push(BilledSegment {
            segment_no: seg_idx as i16,
            plan_id: boundaries[b].plan_id.clone(),
            usage_delta,
            included_units,
            base_fee_cents,
            fx_pico_cents_per_unit: fx,
            start_day,
            end_day,
        });
    }

    // ----- Base-fee remainder-cents rule (CRITICAL-2), applied PER PLAN. Rounding
    // each segment's base fee independently can leave a sub-cent residue versus the
    // exact day-weighted full fee for the total days that plan was active. Anchor
    // each plan's TOTAL prorated base at `round_half_up(full_fee × active_days/dim)`
    // and push the leftover onto that plan's LAST segment, so Σ(prorated base for a
    // plan) is exactly the day-weighted full fee and never exceeds one full fee.
    let plan_ids: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        segments
            .iter()
            .filter(|s| seen.insert(s.plan_id.clone()))
            .map(|s| s.plan_id.clone())
            .collect()
    };
    for plan_id in plan_ids {
        let base_full = plan_prices.get(&plan_id).map(|p| p.base_fee_cents).unwrap_or(0);
        if base_full == 0 {
            continue;
        }
        let idxs: Vec<usize> = segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.plan_id == plan_id)
            .map(|(i, _)| i)
            .collect();
        let active_days: u32 = idxs.iter().map(|&i| segments[i].end_day - segments[i].start_day).sum();
        let target = day_weight(base_full, u64::from(active_days), u64::from(dim));
        let summed: u64 = idxs.iter().map(|&i| segments[i].base_fee_cents).sum();
        // Adjust the plan's LAST segment so the per-plan total hits `target`. The
        // residue is at most a cent or two; `target` is bounded by `base_full`, so
        // the adjusted last-segment base never goes negative for a real fee.
        if let Some(&last) = idxs.last() {
            let others: u64 = summed - segments[last].base_fee_cents;
            segments[last].base_fee_cents = target.saturating_sub(others);
        }
    }

    segments
}

/// A finalize-stage boundary row (internal).
struct BoundaryRow {
    plan_id: String,
    start_snapshot: HashMap<String, i64>,
    start_day: u32,
}

/// The real entry point used by the reconcile + tests: build segments given the
/// plan running at period START (`prior_plan_id`), the ordered change rows, the
/// app's CURRENT plan (the tail's source of truth — MAJOR-4), and the period-end
/// totals. Each change opens a new segment whose plan is the change's `to_plan_id`;
/// the LAST segment's plan is forced to `current_plan_id` so an over-cap flip that
/// moved the running plan past the last recorded snapshot still prices the tail
/// under the actually-running plan.
pub fn build_segments_with_prior(
    period_start_unix: i64,
    prior_plan_id: &str,
    period_changes: &[PlanChange],
    period_end_totals: &HashMap<String, i64>,
    current_plan_id: &str,
    plan_prices: &HashMap<String, PlanPrice>,
) -> Vec<BilledSegment> {
    let dim = days_in_period(period_start_unix);

    let mut boundaries: Vec<BoundaryRow> = Vec::new();
    if period_changes.is_empty() {
        // N=0 ⇒ exactly one segment, full period, current plan (== today).
        boundaries.push(BoundaryRow {
            plan_id: current_plan_id.to_string(),
            start_snapshot: HashMap::new(),
            start_day: 1,
        });
    } else {
        // Segment 0: day 1, cumulative 0, the plan running before the first change.
        boundaries.push(BoundaryRow {
            plan_id: prior_plan_id.to_string(),
            start_snapshot: HashMap::new(),
            start_day: 1,
        });
        for (i, ch) in period_changes.iter().enumerate() {
            // The LAST change opens the final segment, whose plan is forced to the
            // actually-running plan (MAJOR-4).
            let plan_id = if i + 1 == period_changes.len() {
                current_plan_id.to_string()
            } else {
                ch.to_plan_id.clone()
            };
            boundaries.push(BoundaryRow {
                plan_id,
                start_snapshot: ch.usage_at_change.clone(),
                start_day: change_day_of_month(ch.effective_at, period_start_unix),
            });
        }
    }

    finalize_boundaries(boundaries, dim, period_end_totals, plan_prices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{charge_cents, MetricWeight, MetricWeights, PlanPrice, FX_SCALE};

    /// 2026-06-01 UTC — June has 30 days, matching the worked example's 30-day month.
    fn june_2026_start() -> i64 {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap().timestamp()
    }

    /// The 11th of June 2026, 00:00 UTC — the worked example's upgrade instant.
    fn june_11_2026() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 11, 0, 0, 0).unwrap()
    }

    fn one_cent_per_cu() -> u64 {
        FX_SCALE as u64
    }

    fn weights_requests() -> MetricWeights {
        let mut w = MetricWeights::new();
        w.insert("requests".to_string(), MetricWeight { units_per_op: 1, per_units: 1 });
        w
    }

    fn snap(metric: &str, total: i64) -> HashMap<String, i64> {
        let mut m = HashMap::new();
        m.insert(metric.to_string(), total);
        m
    }

    #[test]
    fn days_in_period_is_calendar_days() {
        assert_eq!(days_in_period(june_2026_start()), 30, "June has 30 days");
        let feb = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap().timestamp();
        assert_eq!(days_in_period(feb), 28, "Feb 2026 has 28 days");
        let jan = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap().timestamp();
        assert_eq!(days_in_period(jan), 31, "Jan has 31 days");
    }

    #[test]
    fn no_change_degenerates_to_one_full_period_segment() {
        // N=0 ⇒ exactly one segment_no=0, full quota/base, current plan — byte-for-byte
        // today's single-line behaviour.
        let mut prices = HashMap::new();
        prices.insert(
            "pln_pro".to_string(),
            PlanPrice {
                base_fee_cents: 3000,
                included_units: 10_000,
                fx_pico_cents_per_unit: Some(one_cent_per_cu()),
                spend_limit_default_cents: 0,
            },
        );
        let segs = build_segments_with_prior(
            june_2026_start(),
            "pln_pro",
            &[],
            &snap("requests", 30_000),
            "pln_pro",
            &prices,
        );
        assert_eq!(segs.len(), 1, "no change ⇒ one segment");
        assert_eq!(segs[0].segment_no, 0);
        assert_eq!(segs[0].plan_id, "pln_pro");
        assert_eq!(segs[0].included_units, 10_000, "full quota (30/30)");
        assert_eq!(segs[0].base_fee_cents, 3000, "full base fee (30/30)");
        assert_eq!(segs[0].usage_delta.get("requests"), Some(&30_000), "full-period delta from 0");
    }

    #[test]
    fn worked_example_two_segments_match_derived_numbers() {
        // The doc's worked sub-trace, re-derived from the half-open day rule:
        //   Free [1st,11th) = 10 days; Pro [11th, period_end) = 20 days; 10+20=30.
        //   Free quota = round_half_up(1000 × 10/30) = 333; base 0.
        //   Pro  quota = round_half_up(10000 × 20/30) = 6667; base = 3000×20/30 = 2000.
        let free = PlanPrice {
            base_fee_cents: 0,
            included_units: 1_000,
            fx_pico_cents_per_unit: Some(one_cent_per_cu()),
            spend_limit_default_cents: 0,
        };
        let pro = PlanPrice {
            base_fee_cents: 3_000,
            included_units: 10_000,
            fx_pico_cents_per_unit: Some(one_cent_per_cu()),
            spend_limit_default_cents: 0,
        };
        let mut prices = HashMap::new();
        prices.insert("pln_free".to_string(), free.clone());
        prices.insert("pln_pro".to_string(), pro.clone());

        let changes = vec![PlanChange {
            to_plan_id: "pln_pro".to_string(),
            effective_at: june_11_2026(),
            usage_at_change: snap("requests", 4_000),
        }];
        let segs = build_segments_with_prior(
            june_2026_start(),
            "pln_free", // prior plan (segment 0)
            &changes,
            &snap("requests", 30_000), // period-end cumulative
            "pln_pro",  // current plan (== last change's to-plan)
            &prices,
        );
        assert_eq!(segs.len(), 2, "one change ⇒ two segments");

        // Segment 0 — Free, days 1–10.
        assert_eq!(segs[0].segment_no, 0);
        assert_eq!(segs[0].plan_id, "pln_free");
        assert_eq!(segs[0].start_day, 1);
        assert_eq!(segs[0].end_day, 11, "half-open [1, 11)");
        assert_eq!(segs[0].included_units, 333, "round_half_up(1000×10/30)");
        assert_eq!(segs[0].base_fee_cents, 0);
        assert_eq!(segs[0].usage_delta.get("requests"), Some(&4_000), "max(0, 4000−0)");

        // Segment 1 — Pro, days 11–30.
        assert_eq!(segs[1].segment_no, 1);
        assert_eq!(segs[1].plan_id, "pln_pro");
        assert_eq!(segs[1].start_day, 11);
        assert_eq!(segs[1].end_day, 31, "half-open [11, 31) — period_end = dim+1");
        assert_eq!(segs[1].included_units, 6_667, "round_half_up(10000×20/30)");
        assert_eq!(segs[1].base_fee_cents, 2_000, "3000×20/30");
        assert_eq!(segs[1].usage_delta.get("requests"), Some(&26_000), "max(0, 30000−4000)");

        // Telescoping: deltas sum to the full-period total.
        let total_delta: i64 = segs.iter().map(|s| *s.usage_delta.get("requests").unwrap_or(&0)).sum();
        assert_eq!(total_delta, 30_000, "segment deltas telescope to the period total");

        // Day-partition sums to days_in_period.
        let total_days: u32 = segs.iter().map(|s| s.end_day - s.start_day).sum();
        assert_eq!(total_days, 30, "Σ segment_days == days_in_period");

        // Base-fee invariant: Σ base ≤ one full Pro base fee (and == day-weighted).
        let total_base: u64 = segs.iter().map(|s| s.base_fee_cents).sum();
        assert_eq!(total_base, 2_000, "0 + 2000; ≤ one full $30 fee");

        // Price each segment under ITS OWN plan over its delta — assert the doc's
        // hard-coded amounts via the REAL charge_cents.
        let w = weights_requests();
        let seg0_price = PlanPrice {
            base_fee_cents: segs[0].base_fee_cents,
            included_units: segs[0].included_units,
            fx_pico_cents_per_unit: segs[0].fx_pico_cents_per_unit,
            spend_limit_default_cents: 0,
        };
        let seg1_price = PlanPrice {
            base_fee_cents: segs[1].base_fee_cents,
            included_units: segs[1].included_units,
            fx_pico_cents_per_unit: segs[1].fx_pico_cents_per_unit,
            spend_limit_default_cents: 0,
        };
        let c0 = charge_cents(&seg0_price, &segs[0].usage_delta, &w).unwrap();
        let c1 = charge_cents(&seg1_price, &segs[1].usage_delta, &w).unwrap();
        assert_eq!(c0.total_cents, 3_667, "Free: max(0,4000−333)=3667 CU × 1c + base 0");
        assert_eq!(c1.total_cents, 21_333, "Pro: max(0,26000−6667)=19333 CU × 1c + base 2000");
        assert_eq!(c0.total_cents + c1.total_cents, 25_000, "invoice subtotal $250.00");
    }

    #[test]
    fn end_missing_metric_delta_floors_at_zero() {
        // MAJOR-3: a metric present in the START snapshot but ABSENT at the segment
        // END reads end=0; end−start would be negative; the floor pins it to 0 so a
        // vanished metric never credits the bill.
        let p = PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(one_cent_per_cu()),
            spend_limit_default_cents: 0,
        };
        let mut prices = HashMap::new();
        prices.insert("pln_a".to_string(), p.clone());
        prices.insert("pln_b".to_string(), p.clone());
        let changes = vec![PlanChange {
            to_plan_id: "pln_b".to_string(),
            effective_at: june_11_2026(),
            usage_at_change: snap("retired", 500), // present at the change snapshot
        }];
        // Period-end totals MISSING the `retired` metric (it vanished).
        let segs = build_segments_with_prior(
            june_2026_start(),
            "pln_a",
            &changes,
            &HashMap::new(), // END snapshot has no `retired`
            "pln_b",
            &prices,
        );
        // Segment 1's delta for `retired` = max(0, 0 − 500) = 0 (floored, not −500).
        assert_eq!(segs[1].usage_delta.get("retired"), None, "negative delta floored away");
    }

    #[test]
    fn zero_day_segment_merges_into_neighbour() {
        // MINOR-6: two changes on the SAME calendar day ⇒ the middle segment has 0
        // days; it merges so its usage delta accrues to the surviving neighbour and
        // no quota-starved over-bill line is emitted. Day-partition still sums to dim.
        let p = PlanPrice {
            base_fee_cents: 0,
            included_units: 1_000,
            fx_pico_cents_per_unit: Some(one_cent_per_cu()),
            spend_limit_default_cents: 0,
        };
        let mut prices = HashMap::new();
        for id in ["pln_a", "pln_b", "pln_c"] {
            prices.insert(id.to_string(), p.clone());
        }
        let changes = vec![
            PlanChange {
                to_plan_id: "pln_b".to_string(),
                effective_at: june_11_2026(),
                usage_at_change: snap("requests", 4_000),
            },
            PlanChange {
                to_plan_id: "pln_c".to_string(),
                effective_at: june_11_2026(), // SAME day ⇒ segment 1 is zero-day
                usage_at_change: snap("requests", 5_000),
            },
        ];
        let segs = build_segments_with_prior(
            june_2026_start(),
            "pln_a",
            &changes,
            &snap("requests", 30_000),
            "pln_c",
            &prices,
        );
        // The zero-day middle segment is dropped: 3 boundaries → 2 surviving segments.
        assert_eq!(segs.len(), 2, "zero-day segment merged away");
        let total_days: u32 = segs.iter().map(|s| s.end_day - s.start_day).sum();
        assert_eq!(total_days, 30, "Σ segment_days still == days_in_period after merge");
        // Telescoping preserved across the merge.
        let total_delta: i64 = segs.iter().map(|s| *s.usage_delta.get("requests").unwrap_or(&0)).sum();
        assert_eq!(total_delta, 30_000, "deltas telescope after merge");
    }

    #[test]
    fn base_fee_remainder_lands_on_last_segment() {
        // CRITICAL-2: rounding N segment base fees independently can leave a residue
        // versus the exact day-weighted full fee; the remainder lands on the LAST
        // segment of that plan so Σ(prorated base for a plan) is exact and ≤ one fee.
        // Same plan throughout but split at day 11: base 1000, 30-day month.
        //   seg0 days 1–10: round_half_up(1000×10/30)=round(333.33)=333
        //   seg1 days 11–30: target Σ for the plan = round_half_up(1000×30/30)=1000
        //     ⇒ seg1 base = 1000 − 333 = 667 (NOT round(1000×20/30)=667 — happens to match,
        //     but the remainder rule guarantees Σ == 1000 exactly).
        let same = PlanPrice {
            base_fee_cents: 1_000,
            included_units: 0,
            fx_pico_cents_per_unit: Some(one_cent_per_cu()),
            spend_limit_default_cents: 0,
        };
        let mut prices = HashMap::new();
        prices.insert("pln_x".to_string(), same);
        let changes = vec![PlanChange {
            to_plan_id: "pln_x".to_string(),
            effective_at: june_11_2026(),
            usage_at_change: snap("requests", 0),
        }];
        let segs = build_segments_with_prior(
            june_2026_start(),
            "pln_x",
            &changes,
            &snap("requests", 0),
            "pln_x",
            &prices,
        );
        let total_base: u64 = segs.iter().map(|s| s.base_fee_cents).sum();
        assert_eq!(total_base, 1_000, "Σ prorated base == one full fee exactly (remainder rule)");
        assert!(total_base <= 1_000, "never exceeds one full fee");
    }

    #[test]
    fn next_period_date_wraps_december() {
        let dec = chrono::NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        assert_eq!(next_period_date(dec), chrono::NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
        let jun = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        assert_eq!(next_period_date(jun), chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap());
    }
}
