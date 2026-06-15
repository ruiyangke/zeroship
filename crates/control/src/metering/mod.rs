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

use chrono::{Datelike, NaiveDate, TimeZone, Utc};
use uuid::Uuid;
use zeroship_core::types::{AppUsage, UsageReport};

use crate::registry::{Registry, RegistryError};

pub mod provider;

/// Per-`owner_app` cap on auto-registered `custom` metrics in `billing_metrics`.
/// Bounds attacker-influenced cardinality: an app emitting `usage.custom` keys
/// can register at most this many distinct custom metrics. A NEW custom metric
/// beyond the cap is dropped-with-warn (`custom_metric_cap_refused`) — its delta
/// is NOT applied (so it can never FK-abort the report) and the rest of the
/// report commits normally (at-least-once liveness). Already-registered custom
/// metrics keep flowing (they only bump `last_seen_at`).
pub const MAX_CUSTOM_METRICS_PER_APP: i64 = 100;

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

/// Convert a unix-seconds period start to the `billing_period` DATE value: the
/// first-of-month `NaiveDate` the `period` column (a `zeroship.billing_period`
/// DATE domain) holds. This is THE period representation written to every
/// period-keyed billing table — killing the old `f64`/`TIMESTAMPTZ` round-trip.
///
/// BIND SITE NOTE: the `period` column is a DATE *domain*, so the compio-postgres
/// driver infers the parameter type as the domain OID, which `NaiveDate`'s
/// `ToSql::accepts` rejects. Every WRITE therefore binds via an explicit
/// `$N::date` cast (the date→domain assignment cast then applies). READs need no
/// cast — Postgres reports a domain column's type as its base DATE in the row
/// description, so `NaiveDate`'s `FromSql` accepts it directly.
pub(crate) fn period_date(period_start_unix_secs: i64) -> NaiveDate {
    let dt = Utc
        .timestamp_opt(period_start_unix_secs, 0)
        .single()
        .unwrap_or_else(Utc::now);
    NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1)
        .unwrap_or_else(|| Utc::now().date_naive().with_day(1).unwrap_or(dt.date_naive()))
}

/// Iterate an `AppUsage` as `(metric, delta)` pairs — the five fixed
/// counters (only when non-zero) followed by each custom metric. Used by
/// both the pure ledger and the PG UPSERT so they aggregate identically.
///
/// MINOR (u64→i64 ingest wrap): the counters are `u64` but `usage_aggregates`
/// stores `BIGINT` (i64). A naive `v as i64` of a value above `i64::MAX` wraps
/// NEGATIVE and would LOWER the aggregate — a buggy/compromised worker could
/// poison a tenant's total via `/internal/usage`. We `i64::try_from` and
/// SKIP-with-`warn!` an out-of-range delta rather than wrap it (a single
/// >i64::MAX counter in one flush is itself absurd — ~9.2e18 — so dropping it is
/// strictly safer than wrapping it negative).
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
            push_delta(&mut out, name.to_string(), v);
        }
    }
    for (name, &v) in &usage.custom {
        if v > 0 {
            push_delta(&mut out, name.clone(), v);
        }
    }
    out
}

/// Push a `(metric, delta)` pair, converting the `u64` counter to the `i64`
/// aggregate column. A value above `i64::MAX` is skipped with a `warn!` rather
/// than wrapped negative (see [`usage_deltas`]).
fn push_delta(out: &mut Vec<(String, i64)>, metric: String, v: u64) {
    match i64::try_from(v) {
        Ok(delta) => out.push((metric, delta)),
        Err(_) => {
            tracing::warn!(
                metric = %metric,
                value = v,
                "metering: usage delta exceeds i64::MAX — skipping (refusing to wrap negative)"
            );
        }
    }
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
            // 2. Apply deltas — UPSERT add-to-total per metric. `period` is the
            // first-of-month DATE (the `billing_period` domain); bound via an
            // explicit `$N::date` cast (the driver infers a domain param OID
            // that NaiveDate's ToSql rejects — see `period_date`).
            let period = period_date(period_start_unix_secs);
            for (app_id, usage) in &report.counters {
                // FK-ABORT GUARD (Key flow A): every `usage_aggregates.metric`
                // FKs `billing_metrics(metric) ON DELETE RESTRICT`, so a metric
                // must be cataloged BEFORE its aggregate UPSERT or the whole
                // report tx FK-aborts (and at-least-once retries can never make
                // progress). Platform/primitive metrics are seeded; custom SDK
                // metrics auto-register here — capped per `owner_app`. A NEW
                // custom metric beyond the cap is REFUSED (not inserted): we then
                // DROP its delta (`custom_metric_cap_refused`) so it never reaches
                // the FK-guarded UPSERT, and the rest of the report still applies.
                let deltas = usage_deltas(usage);
                let resolved =
                    Self::register_custom_metrics(&tx, app_id, &deltas).await?;
                for (metric, delta) in deltas {
                    if !resolved.contains(&metric) {
                        // Refused at the cap (or otherwise unresolved): dropped
                        // with a warn, NOT applied — preserves ingest liveness.
                        tracing::warn!(
                            app_id = %app_id,
                            metric = %metric,
                            billing_event = "custom_metric_cap_refused",
                            "metering: custom metric refused at per-app cap — dropping its delta (report still applies)"
                        );
                        continue;
                    }
                    tx.execute(
                        "INSERT INTO zeroship.usage_aggregates AS u \
                           (app_id, period, metric, total, updated_at) \
                         VALUES ($1, $2::date, $3, $4, NOW()) \
                         ON CONFLICT (app_id, period, metric) \
                         DO UPDATE SET total = u.total + EXCLUDED.total, updated_at = NOW()",
                        &[app_id, &period, &metric, &delta],
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

    /// Register (or bump the GC clock of) the `custom` metrics in `deltas` for
    /// `owner_app`, and return the SET of `deltas` metrics that resolve in
    /// `billing_metrics` AFTER this step — i.e. the metrics whose aggregate
    /// UPSERT will not FK-abort.
    ///
    /// Platform/primitive metrics (the fixed counters) are always seeded, so
    /// they resolve unconditionally. A custom metric is resolved iff it is
    /// already cataloged OR it registers now; a NEW custom metric that would push
    /// `owner_app` past [`MAX_CUSTOM_METRICS_PER_APP`] is REFUSED (capped) and is
    /// therefore NOT in the returned set — the caller drops its delta with a warn
    /// rather than FK-aborting the whole report.
    ///
    /// Each registration is `INSERT … ON CONFLICT (metric) DO UPDATE SET
    /// last_seen_at = NOW()`, so an already-known custom metric just bumps its GC
    /// clock. The per-app cap is enforced by an `INSERT … SELECT … WHERE
    /// (count of this app's custom rows) < cap` guard, made race-safe inside the
    /// report tx.
    async fn register_custom_metrics<C: compio_postgres::GenericClient + Sync>(
        conn: &C,
        owner_app: &Uuid,
        deltas: &[(String, i64)],
    ) -> Result<std::collections::HashSet<String>, RegistryError> {
        let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (metric, _) in deltas {
            // Already cataloged (platform/primitive seed OR a prior custom
            // registration)? Then it resolves; bump last_seen_at if it is a
            // custom row owned by this app (cheap, idempotent) and move on.
            let existing = conn
                .query(
                    "SELECT kind::text AS kind FROM zeroship.billing_metrics WHERE metric = $1",
                    &[metric],
                )
                .await?;
            if let Some(row) = existing.first() {
                let kind: String = row.get("kind");
                if kind == "custom" {
                    // Bump the GC clock; ON CONFLICT keeps it idempotent.
                    conn.execute(
                        "UPDATE zeroship.billing_metrics SET last_seen_at = NOW() WHERE metric = $1",
                        &[metric],
                    )
                    .await?;
                }
                resolved.insert(metric.clone());
                continue;
            }
            // Not cataloged ⇒ a NEW custom metric. Register it ONLY if this app is
            // under its cap. The guarded INSERT counts the app's existing custom
            // rows in the same statement, so two concurrent first-sights of the
            // same app can't both slip past the cap (serialized by the report tx).
            let inserted = conn
                .query(
                    "INSERT INTO zeroship.billing_metrics (metric, kind, unit, owner_app, last_seen_at) \
                     SELECT $1, 'custom', 'unit', $2, NOW() \
                     WHERE (SELECT COUNT(*) FROM zeroship.billing_metrics \
                              WHERE owner_app = $2 AND kind = 'custom') < $3 \
                     ON CONFLICT (metric) DO UPDATE SET last_seen_at = NOW() \
                     RETURNING metric",
                    &[metric, owner_app, &MAX_CUSTOM_METRICS_PER_APP],
                )
                .await?;
            if inserted.is_empty() {
                // Cap reached — refused. Leave it OUT of `resolved` so the caller
                // drops the delta (no FK abort).
                continue;
            }
            resolved.insert(metric.clone());
        }
        Ok(resolved)
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
        Self::period_totals_on(&conn, app_id, period_start_unix_secs).await
    }

    /// As [`Metering::period_totals`] but on a BORROWED connection, so a caller
    /// that already holds a connection (e.g. the metering-export sweep, which
    /// reads many app reads per tick) does not pay a fresh per-query connection
    /// handshake. Same query + result shape.
    pub async fn period_totals_on<C: compio_postgres::GenericClient + Sync>(
        conn: &C,
        app_id: &Uuid,
        period_start_unix_secs: i64,
    ) -> Result<HashMap<String, i64>, RegistryError> {
        let rows = conn
            .query(
                "SELECT metric, total FROM zeroship.usage_aggregates \
                 WHERE app_id = $1 AND period = $2::date",
                &[app_id, &period_date(period_start_unix_secs)],
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
                 WHERE app_id = $1 AND period = $2::date \
                   AND metric = $3",
                &[app_id, &period_date(period_start_unix_secs), &metric],
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
    fn ingest_skips_overflow_delta_instead_of_wrapping_negative() {
        // MINOR (u64→i64 ingest wrap) REGRESSION: a counter above i64::MAX must
        // NOT wrap negative and lower the aggregate. Pre-fix `v as i64` of
        // u64::MAX yields -1, so the total would go NEGATIVE; post-fix the
        // out-of-range delta is dropped (skip-with-warn) and the total holds.
        let mut led = IngestLedger::new();
        let app = Uuid::new_v4();
        let p = 1_000;
        // requests fits, cpu_us overflows i64.
        led.ingest_at(
            &report(
                "w1",
                1,
                app,
                AppUsage { requests: 5, cpu_us: u64::MAX, ..Default::default() },
            ),
            p,
        );
        assert_eq!(led.total(app, p, "requests"), 5, "in-range counter still applies");
        assert_eq!(
            led.total(app, p, "cpu_us"),
            0,
            "an overflowing counter is dropped, NOT wrapped negative",
        );
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
