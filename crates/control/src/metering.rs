//! Metering — idempotent usage ingest + period aggregation.
//!
//! Workers flush `UsageReport { worker_id, report_id, sequence, counters }`
//! to `/internal/usage` AT LEAST ONCE (a timed-out POST is retried next
//! tick). This module ingests them idempotently and aggregates per
//! `(app_id, billing-period)`:
//!
//! - **Idempotency:** dedup on `(worker_id, sequence)` via
//!   `zeroship.usage_reports_seen`. The first time a `(worker_id, sequence)`
//!   is seen, the report's deltas are applied; a duplicate is a no-op (it
//!   never reaches the aggregate UPSERT). `ingest` returns the high-water
//!   sequence for the worker so a producer can resync.
//! - **Aggregation:** per `(app_id, period_start, metric)` UPSERT
//!   `total = total + delta` into `zeroship.usage_aggregates`, for the five
//!   fixed platform counters AND each `custom` metric. `period_start` is the
//!   calendar-month boundary (00:00:00 UTC on the 1st) — a new month lands
//!   in a new row automatically (month rollover, salvaged from the
//!   since-deleted `crates/platform` `metering/rollover.rs` period-key logic).
//!
//! Storage is Postgres via `compio-postgres` (zero tokio). The dedup + apply
//! is done in ONE transaction per report so a crash mid-apply can't leave a
//! report marked-seen-but-not-applied (or vice versa).
//!
//! The pure-logic core ([`IngestLedger`]) carries the dedup + aggregation
//! semantics with no DB, so the key correctness property — a duplicate
//! report does not double-count — is unit-testable without a live Postgres.
//! The PG path ([`Metering`]) implements the same semantics against the
//! real tables.

use std::collections::HashMap;

use chrono::{Datelike, TimeZone, Utc};
use uuid::Uuid;
use zeroship_core::types::{AppUsage, UsageReport};

use crate::registry::{Registry, RegistryError};

/// Compute the calendar-month period start (00:00:00 UTC on the 1st) for a
/// given unix-seconds instant, as unix seconds. This is the aggregation
/// bucket key: every report received within a UTC month accumulates into the
/// same `period_start`, and the first report of the next month opens a new
/// bucket.
#[must_use]
pub fn period_start_unix(now_unix: i64) -> i64 {
    let dt = Utc.timestamp_opt(now_unix, 0).single().unwrap_or_else(Utc::now);
    // First of the month, midnight UTC.
    Utc.with_ymd_and_hms(dt.year(), dt.month(), 1, 0, 0, 0)
        .single()
        .map_or(now_unix, |d| d.timestamp())
}

/// The current calendar-month period start as unix seconds.
#[must_use]
pub fn current_period_start_unix() -> i64 {
    period_start_unix(Utc::now().timestamp())
}

/// Iterate an `AppUsage` as `(metric, delta)` pairs — the five fixed
/// counters (only when non-zero) followed by each custom metric. Used by
/// both the pure ledger and the PG UPSERT so they aggregate identically.
fn usage_deltas(usage: &AppUsage) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = Vec::new();
    let fixed = [
        ("requests", usage.requests),
        ("cpu_us", usage.cpu_us),
        ("wall_us", usage.wall_us),
        ("egress_bytes", usage.egress_bytes),
        ("ingress_bytes", usage.ingress_bytes),
    ];
    for (name, v) in fixed {
        if v > 0 {
            out.push((name.to_string(), v as i64));
        }
    }
    for (name, &v) in &usage.custom {
        if v > 0 {
            out.push((name.clone(), v as i64));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Pure-logic ingest ledger (no DB) — the testable core of the idempotency +
// aggregation semantics.
// ---------------------------------------------------------------------------

/// In-memory model of the ingest semantics: a `(worker_id, sequence)` dedup
/// set + a `(app_id, period_start, metric)` total map. `Metering` (the PG
/// path) mirrors this exactly against the real tables. Tests assert on this
/// to pin the duplicate-no-double-count and month-rollover properties
/// without a live Postgres.
#[derive(Debug, Default)]
pub struct IngestLedger {
    /// Dedup ledger: the set of `(worker_id, sequence)` already applied.
    seen: std::collections::HashSet<(String, u64)>,
    /// High-water sequence per worker_id (max applied sequence).
    high_water: HashMap<String, u64>,
    /// Aggregated totals keyed by (app_id, period_start, metric).
    totals: HashMap<(Uuid, i64, String), i64>,
}

impl IngestLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest one report at a fixed `period_start`. Returns the worker's
    /// high-water sequence after this call. A duplicate `(worker_id,
    /// sequence)` is a no-op (totals unchanged) — the idempotency property.
    pub fn ingest_at(&mut self, report: &UsageReport, period_start: i64) -> u64 {
        let key = (report.worker_id.clone(), report.sequence);
        if !self.seen.insert(key) {
            // Already applied — no-op. Return the current high-water mark.
            return self.high_water.get(&report.worker_id).copied().unwrap_or(0);
        }
        for (app_id, usage) in &report.counters {
            for (metric, delta) in usage_deltas(usage) {
                *self
                    .totals
                    .entry((*app_id, period_start, metric))
                    .or_insert(0) += delta;
            }
        }
        let hw = self
            .high_water
            .entry(report.worker_id.clone())
            .or_insert(0);
        *hw = (*hw).max(report.sequence);
        *hw
    }

    /// Total for a `(app_id, period_start, metric)` bucket (0 if absent).
    #[must_use]
    pub fn total(&self, app_id: Uuid, period_start: i64, metric: &str) -> i64 {
        self.totals
            .get(&(app_id, period_start, metric.to_string()))
            .copied()
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Postgres-backed metering (the real path)
// ---------------------------------------------------------------------------

/// Idempotent, period-aggregating usage ingest backed by Postgres
/// (`compio-postgres`). Holds a `Registry` so it shares the control plane's
/// connection model.
#[derive(Clone, Debug)]
pub struct Metering {
    registry: Registry,
}

impl Metering {
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// Ingest one report idempotently at the current calendar-month period.
    /// See [`Self::ingest_at`].
    pub async fn ingest(&self, report: &UsageReport) -> Result<IngestOutcome, RegistryError> {
        self.ingest_at(report, current_period_start_unix()).await
    }

    /// Ingest one report idempotently at an explicit `period_start` (unix
    /// seconds; the calendar-month boundary). One transaction:
    ///
    /// 1. `INSERT … usage_reports_seen(worker_id, sequence) ON CONFLICT DO
    ///    NOTHING` — affecting 0 rows means "already seen": commit and return
    ///    `Duplicate` WITHOUT applying any delta (the no-double-count
    ///    guarantee).
    /// 2. Otherwise UPSERT each `(app_id, period_start, metric)` total.
    ///
    /// Returns the worker's high-water sequence either way so a producer can
    /// resync after a restart.
    pub async fn ingest_at(
        &self,
        report: &UsageReport,
        period_start_unix_secs: i64,
    ) -> Result<IngestOutcome, RegistryError> {
        let mut conn = self.registry.conn().await?;
        let tx = conn.transaction().await?;

        // 1. Dedup gate. RETURNING tells us whether the row was new.
        let seq_i64 = report.sequence as i64;
        let inserted = tx
            .query(
                "INSERT INTO zeroship.usage_reports_seen (worker_id, sequence) \
                 VALUES ($1, $2) ON CONFLICT (worker_id, sequence) DO NOTHING \
                 RETURNING sequence",
                &[&report.worker_id, &seq_i64],
            )
            .await?;
        let is_new = !inserted.is_empty();

        if is_new {
            // 2. Apply deltas — UPSERT add-to-total per metric. period_start
            // bound as unix seconds via to_timestamp (same idiom as
            // stripe_store::record_payout — avoids TIMESTAMPTZ binding).
            for (app_id, usage) in &report.counters {
                for (metric, delta) in usage_deltas(usage) {
                    tx.execute(
                        "INSERT INTO zeroship.usage_aggregates AS u \
                           (app_id, period_start, metric, total, updated_at) \
                         VALUES ($1, to_timestamp($2::double precision), $3, $4, NOW()) \
                         ON CONFLICT (app_id, period_start, metric) \
                         DO UPDATE SET total = u.total + EXCLUDED.total, updated_at = NOW()",
                        &[app_id, &(period_start_unix_secs as f64), &metric, &delta],
                    )
                    .await?;
                }
            }
        }

        tx.commit().await?;

        // High-water sequence for this worker (after this report).
        let hw = self.high_water_sequence(&report.worker_id).await?;
        Ok(IngestOutcome {
            duplicate: !is_new,
            high_water_sequence: hw,
        })
    }

    /// The maximum applied sequence for a worker (0 if none). A producer
    /// uses this to resync its sequence after a crash/restart.
    pub async fn high_water_sequence(&self, worker_id: &str) -> Result<u64, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT COALESCE(MAX(sequence), 0)::bigint AS hw \
                 FROM zeroship.usage_reports_seen WHERE worker_id = $1",
                &[&worker_id],
            )
            .await?;
        let hw: i64 = rows.first().map_or(0, |r| r.get("hw"));
        Ok(hw.max(0) as u64)
    }

    /// All aggregated metric totals for an app in a given period, as a
    /// `metric → total` map. Backs the creator-facing usage read.
    pub async fn period_totals(
        &self,
        app_id: &Uuid,
        period_start_unix_secs: i64,
    ) -> Result<HashMap<String, i64>, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT metric, total FROM zeroship.usage_aggregates \
                 WHERE app_id = $1 AND period_start = to_timestamp($2::double precision)",
                &[app_id, &(period_start_unix_secs as f64)],
            )
            .await?;
        let mut out = HashMap::new();
        for row in &rows {
            out.insert(row.get::<_, String>("metric"), row.get::<_, i64>("total"));
        }
        Ok(out)
    }

    /// All aggregated metric totals for an app in the CURRENT calendar-month
    /// period.
    pub async fn current_period_totals(
        &self,
        app_id: &Uuid,
    ) -> Result<HashMap<String, i64>, RegistryError> {
        self.period_totals(app_id, current_period_start_unix()).await
    }

    /// Read the aggregated total for a `(app_id, period_start, metric)`
    /// bucket. Used by tests + future pricing.
    pub async fn total(
        &self,
        app_id: &Uuid,
        period_start_unix_secs: i64,
        metric: &str,
    ) -> Result<i64, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT total FROM zeroship.usage_aggregates \
                 WHERE app_id = $1 AND period_start = to_timestamp($2::double precision) \
                   AND metric = $3",
                &[app_id, &(period_start_unix_secs as f64), &metric],
            )
            .await?;
        Ok(rows.first().map_or(0, |r| r.get("total")))
    }
}

/// Result of an ingest: whether it was a duplicate (no-op) and the worker's
/// current high-water sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestOutcome {
    pub duplicate: bool,
    pub high_water_sequence: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(worker: &str, seq: u64, app: Uuid, usage: AppUsage) -> UsageReport {
        let mut counters = HashMap::new();
        counters.insert(app, usage);
        UsageReport {
            worker_id: worker.to_string(),
            report_id: Uuid::now_v7(),
            sequence: seq,
            counters,
        }
    }

    // -- period boundary -----------------------------------------------------

    #[test]
    fn period_start_is_first_of_month_utc() {
        // 2026-06-13T12:34:56Z → 2026-06-01T00:00:00Z.
        let mid_june = Utc.with_ymd_and_hms(2026, 6, 13, 12, 34, 56).unwrap().timestamp();
        let ps = period_start_unix(mid_june);
        let expected = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap().timestamp();
        assert_eq!(ps, expected);
    }

    #[test]
    fn period_start_differs_across_month_boundary() {
        let jun = period_start_unix(Utc.with_ymd_and_hms(2026, 6, 30, 23, 59, 59).unwrap().timestamp());
        let jul = period_start_unix(Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap().timestamp());
        assert_ne!(jun, jul, "June and July must land in different periods");
        assert_eq!(jul, Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap().timestamp());
    }

    // -- idempotency (the key property) -------------------------------------

    #[test]
    fn ingest_aggregates_deltas() {
        let mut led = IngestLedger::new();
        let app = Uuid::new_v4();
        let p = 1_000;
        led.ingest_at(&report("w1", 1, app, AppUsage { requests: 10, ..Default::default() }), p);
        led.ingest_at(&report("w1", 2, app, AppUsage { requests: 5, ..Default::default() }), p);
        assert_eq!(led.total(app, p, "requests"), 15, "two distinct reports sum");
    }

    #[test]
    fn duplicate_report_does_not_double_count() {
        // THE idempotency guarantee: re-ingesting the SAME (worker_id,
        // sequence) must NOT add its deltas again. (RED before the dedup
        // gate existed: the second ingest would push the total to 20.)
        let mut led = IngestLedger::new();
        let app = Uuid::new_v4();
        let p = 1_000;
        let r = report("w1", 1, app, AppUsage { requests: 10, ..Default::default() });

        let hw1 = led.ingest_at(&r, p);
        let hw2 = led.ingest_at(&r, p); // exact duplicate (at-least-once retry)

        assert_eq!(led.total(app, p, "requests"), 10, "duplicate must not double-count");
        assert_eq!(hw1, 1);
        assert_eq!(hw2, 1, "high-water unchanged by a duplicate");
    }

    #[test]
    fn distinct_workers_same_sequence_both_apply() {
        // The dedup key is (worker_id, sequence) — two DIFFERENT workers
        // each emitting sequence 1 are NOT duplicates of each other.
        let mut led = IngestLedger::new();
        let app = Uuid::new_v4();
        let p = 1_000;
        led.ingest_at(&report("w1", 1, app, AppUsage { requests: 3, ..Default::default() }), p);
        led.ingest_at(&report("w2", 1, app, AppUsage { requests: 4, ..Default::default() }), p);
        assert_eq!(led.total(app, p, "requests"), 7, "different workers both count");
    }

    #[test]
    fn custom_metrics_aggregate_alongside_fixed() {
        let mut led = IngestLedger::new();
        let app = Uuid::new_v4();
        let p = 1_000;
        let mut custom = HashMap::new();
        custom.insert("emails_sent".to_string(), 4u64);
        led.ingest_at(
            &report("w1", 1, app, AppUsage { requests: 2, custom, ..Default::default() }),
            p,
        );
        assert_eq!(led.total(app, p, "requests"), 2);
        assert_eq!(led.total(app, p, "emails_sent"), 4);
    }

    #[test]
    fn month_rollover_puts_new_period_in_new_bucket() {
        let mut led = IngestLedger::new();
        let app = Uuid::new_v4();
        let june = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap().timestamp();
        let july = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap().timestamp();

        led.ingest_at(&report("w1", 1, app, AppUsage { requests: 10, ..Default::default() }), june);
        led.ingest_at(&report("w1", 2, app, AppUsage { requests: 7, ..Default::default() }), july);

        assert_eq!(led.total(app, june, "requests"), 10, "June bucket isolated");
        assert_eq!(led.total(app, july, "requests"), 7, "July bucket isolated");
    }

    #[test]
    fn high_water_tracks_max_sequence() {
        let mut led = IngestLedger::new();
        let app = Uuid::new_v4();
        let p = 1_000;
        assert_eq!(led.ingest_at(&report("w1", 5, app, AppUsage::default()), p), 5);
        // An out-of-order lower sequence still applies (new key) but the
        // high-water mark does not regress.
        assert_eq!(led.ingest_at(&report("w1", 3, app, AppUsage::default()), p), 5);
        assert_eq!(led.ingest_at(&report("w1", 9, app, AppUsage::default()), p), 9);
    }
}
