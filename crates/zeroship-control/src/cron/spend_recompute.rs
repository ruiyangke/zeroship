//! Periodic spend recompute: retained stream → `usage_aggregates` snapshot →
//! existing spend evaluation.
//!
//! v7 enforcement intentionally runs at a tunable cadence (default hourly). The
//! recompute reads the current billing period from the retained usage-event
//! stream, computes a full `SUM(value)` snapshot per `(app_id, metric)`, writes
//! that snapshot into `usage_aggregates`, and then triggers the existing spend
//! evaluator so the gateway can keep enforcing `app_spend_state`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use zeroship_core::app_id::AppId;
use zeroship_core::usage_event::UsageEvent;
use zeroship_stream::{
    unless_stalled, Clock, PartitionEnd, StreamRecord, StreamTransport, SystemClock,
};

use crate::metering::{period_start_unix, Metering, UsageAggregate};
use crate::registry::{Registry, RegistryError};
use crate::AppState;

pub const DEFAULT_RECOMPUTE_INTERVAL_SECS: u64 = 60 * 60;
pub const DEFAULT_BATCH_MAX: usize = 10_000;

#[derive(Debug, Clone)]
pub struct SpendRecomputeConfig {
    pub interval: Duration,
    pub settle_window: Duration,
    pub batch_max: usize,
    /// Source of elapsed time for the stall-timeout decision in
    /// [`recompute_usage_aggregates`]. [`SystemClock`] by default; a test
    /// overrides it with a clock it advances itself, to drive that decision
    /// deterministically instead of depending on real timer latency under
    /// load. Carried here, on the scan's caller, rather
    /// than on the [`StreamTransport`] every adapter implements, since only
    /// the scan itself needs it. Public like this struct's other fields.
    pub clock: Arc<dyn Clock>,
}

impl Default for SpendRecomputeConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_RECOMPUTE_INTERVAL_SECS),
            settle_window: Duration::from_secs(
                super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: DEFAULT_BATCH_MAX,
            clock: Arc::new(SystemClock),
        }
    }
}

impl SpendRecomputeConfig {
    /// Override the clock [`recompute_usage_aggregates`] reads elapsed time
    /// from. Production never calls this; a test injects a clock it advances
    /// itself, so a scripted stream that paces delivery can make the
    /// stall-timeout decision deterministic instead of exposed to real
    /// scheduler latency under load.
    #[cfg(test)]
    #[must_use]
    fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SpendRecomputeCycle {
    /// Records read inside the ends captured when the scan started, each
    /// offset once. A record appended after the capture, or delivered again
    /// for an offset already read, is not counted.
    pub polled: usize,
    pub decoded: usize,
    /// Events read but not applied to this period's totals, split by CAUSE.
    ///
    /// These were one `skipped` counter, and the aggregate could not answer
    /// the question it looked like it answered. The recompute rewinds and
    /// re-reads the WHOLE retained stream for every period it rebuilds, so
    /// each event belonging to any OTHER period is "skipped" on every cycle -
    /// on a mature stream that is nearly the entire history, every time. A
    /// genuinely late arrival, which is the case worth alerting on, added 1 to
    /// a number already in the millions.
    ///
    /// `other_period` is EXPECTED to be large and is not an error signal by
    /// itself; it is separated so it stops drowning the other three.
    pub skipped_other_period: usize,
    /// Duplicate `event_id` seen within one cycle - the at-least-once stream
    /// delivering a replay. Small and non-zero is normal.
    pub skipped_duplicate: usize,
    /// Decoded and in-period but carrying no app subject, so it cannot be
    /// attributed. Should be zero; non-zero is usage nobody gets billed for.
    pub skipped_no_app: usize,
    /// In-period and attributable but worth zero. Harmless.
    pub skipped_zero_value: usize,
    pub skipped_undecodable: usize,
    pub aggregates: usize,
    pub written: usize,
    pub transitions: usize,
}

/// Why `apply_event` did or did not fold an event into the running totals.
///
/// Replaces a `bool`. The boolean forced every non-application into one
/// counter, and the four causes have nothing in common operationally: one is
/// expected in bulk, one is expected in trickle, one should never happen, and
/// one is noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Applied {
    Yes,
    /// `event_time` belongs to a different billing period than the one being
    /// rebuilt. Dominant by construction on any non-empty stream.
    OtherPeriod,
    /// In-period but no `subject.app`, so it cannot be attributed to anyone.
    NoAppSubject,
    /// In-period and attributable but zero-valued.
    ZeroValue,
}

#[derive(Debug, thiserror::Error)]
pub enum SpendRecomputeError {
    #[error("stream: {0}")]
    Stream(#[from] zeroship_stream::StreamError),
    #[error("decode usage event at partition {partition} offset {offset}: {source}")]
    Decode {
        partition: i32,
        offset: i64,
        source: serde_json::Error,
    },
    #[error("registry: {0}")]
    Registry(String),
    /// The full-period scan went the transport's stall timeout without reading
    /// a record that brought a partition closer to the end captured when the
    /// scan started: a poll that parked, or polls that kept answering without
    /// advancing. The scan is abandoned and writes nothing, so the last
    /// written snapshot stays in force; the enforcement loop logs this and
    /// runs a fresh scan on its next interval.
    ///
    /// `stall_ms` is the transport's stall timeout. It is zero for an
    /// in-process transport, where one poll that advanced nothing is already
    /// the stall. `unfinished` names every partition still short of its
    /// captured end, with the end this scan captured and how far it actually
    /// got, so a log line tells an operator where the scan stopped rather than
    /// only how many partitions were left.
    #[error(
        "stream scan stalled: no record advanced toward the captured end within {stall_ms}ms; \
         unfinished partitions: {}; the snapshot was not written",
        format_unfinished_partitions(.unfinished)
    )]
    Stalled {
        stall_ms: u64,
        unfinished: Vec<UnfinishedPartition>,
    },
}

/// Where one still-unfinished partition sat when a scan gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnfinishedPartition {
    pub partition: i32,
    /// The captured offset of the partition's last record: the scan's end.
    pub captured_end: i64,
    /// The highest offset folded so far, `None` before the first.
    pub last_read: Option<i64>,
}

impl std::fmt::Display for UnfinishedPartition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "partition {} (captured end {}, ", self.partition, self.captured_end)?;
        match self.last_read {
            Some(read) => write!(f, "last read {read})"),
            None => write!(f, "nothing read yet)"),
        }
    }
}

fn format_unfinished_partitions(unfinished: &[UnfinishedPartition]) -> String {
    unfinished
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl From<RegistryError> for SpendRecomputeError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value.to_string())
    }
}

/// Cron entry point. A stream or database error, or a scan that stalls, is
/// logged and retried after the next cadence.
///
/// Every cycle's scan either completes or writes nothing.
/// [`recompute_usage_aggregates`] captures each partition's end right after
/// rewinding and reads every partition up to that captured end;
/// `replace_period_snapshot`, which DELETEs the whole period and rewrites it,
/// runs only once every partition got there. Records produced after the
/// capture belong to the next cycle, so a producer writing as fast as the scan
/// reads cannot stretch it, and a scan of any size that keeps advancing runs
/// to the end. A scan that goes the transport's stall timeout without
/// advancing returns [`SpendRecomputeError::Stalled`] and leaves the last
/// written snapshot in force.
#[allow(clippy::future_not_send)]
pub async fn run(
    state: Arc<AppState>,
    stream: Arc<dyn StreamTransport>,
    cfg: SpendRecomputeConfig,
) {
    tracing::info!(
        interval_secs = cfg.interval.as_secs(),
        batch_max = cfg.batch_max,
        stream = stream.id(),
        "control spend_recompute cron starting"
    );
    loop {
        match tick(&state, stream.as_ref(), &cfg).await {
            Ok(cycle) => {
                tracing::info!(
                    polled = cycle.polled,
                    decoded = cycle.decoded,
                    skipped_other_period = cycle.skipped_other_period,
                    skipped_duplicate = cycle.skipped_duplicate,
                    skipped_no_app = cycle.skipped_no_app,
                    skipped_zero_value = cycle.skipped_zero_value,
                    skipped_undecodable = cycle.skipped_undecodable,
                    aggregates = cycle.aggregates,
                    written = cycle.written,
                    transitions = cycle.transitions,
                    "control spend_recompute cycle completed"
                );
            }
            Err(err) => {
                tracing::error!(error = %err, "control spend_recompute cycle failed");
            }
        }
        compio::time::sleep(cfg.interval).await;
    }
}

/// Run one current-period recompute and then trigger the existing spend
/// evaluator path (`spend_reconcile::tick`, which calls
/// `SpendEngine::evaluate_all` under its own multi-instance advisory lock).
///
/// The recompute half takes an advisory lock, like every other periodic sweep
/// in this crate (billing_reconcile, stripe_reconcile, billing_notify and
/// workflow_blob_gc all single-flight). Each replica has its own consumer
/// group, so every replica reads the COMPLETE retained stream and would
/// otherwise write the same snapshot: correct, but N replicas each doing
/// O(period) work per tick with N concurrent `replace_period_snapshot`
/// transactions contending on the same rows.
///
/// Single-flighting is safe because a loser's group has no other members, so
/// skipping its poll leaves nothing unread.
///
/// The lock wraps only the recompute. `spend_reconcile::tick` keeps its own.
#[allow(clippy::future_not_send)]
pub async fn tick(
    state: &AppState,
    stream: &dyn StreamTransport,
    cfg: &SpendRecomputeConfig,
) -> Result<SpendRecomputeCycle, SpendRecomputeError> {
    let lock_conn = state.registry.conn().await?;
    let got = lock_conn
        .query(
            "SELECT pg_try_advisory_lock($1) AS locked",
            &[&super::lock_keys::SPEND_RECOMPUTE],
        )
        .await
        .map_err(|e| SpendRecomputeError::Registry(e.to_string()))?;
    let acquired = got.first().is_some_and(|r| r.get::<_, bool>("locked"));
    if !acquired {
        tracing::debug!(
            "spend-recompute: advisory lock held by another instance - skipping the recompute"
        );
        // Still drive the evaluator: it single-flights on its OWN key, so this
        // is not a second unguarded path, and a replica that loses the
        // recompute race should not also sit out enforcement.
        let cycle = SpendRecomputeCycle {
            transitions: super::spend_reconcile::tick(state).await?,
            ..Default::default()
        };
        return Ok(cycle);
    }

    let result = recompute_and_reconcile(state, stream, cfg).await;

    if let Err(e) = lock_conn
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&super::lock_keys::SPEND_RECOMPUTE],
        )
        .await
    {
        tracing::warn!(error = %e, "spend-recompute: advisory unlock failed (lock frees on conn drop)");
    }

    result
}

/// The lock-held body. Separate so the lock in [`tick`] wraps exactly this and
/// so an integration test can drive the work directly, bypassing the
/// single-flight - the same split `billing_notify` uses.
///
/// `spend_reconcile::tick` runs whatever the recompute returned, not only when
/// it succeeds: a scan that stalls still leaves the last written snapshot in
/// force, and a limit lowered below that snapshot's usage must not wait on a
/// future complete scan to take effect. The recompute's own error, if any, is
/// still returned once the evaluator has run, so a stall stays visible to the
/// cron loop's logging instead of reading as a quiet success - and if the
/// evaluator ALSO fails, only its error can propagate from this function, so
/// the recompute's own error (a stall's partition detail included) is logged
/// here first rather than silently dropped.
#[allow(clippy::future_not_send)]
pub async fn recompute_and_reconcile(
    state: &AppState,
    stream: &dyn StreamTransport,
    cfg: &SpendRecomputeConfig,
) -> Result<SpendRecomputeCycle, SpendRecomputeError> {
    let recomputed = recompute_unsettled_period_snapshots(
        &state.registry,
        stream,
        chrono::Utc::now().timestamp(),
        cfg,
    )
    .await;
    let transitions = match super::spend_reconcile::tick(state).await {
        Ok(transitions) => transitions,
        Err(evaluator_error) => {
            log_recompute_error_lost_to_evaluator_failure(&recomputed);
            return Err(evaluator_error.into());
        }
    };
    let mut cycle = recomputed?;
    cycle.transitions = transitions;
    Ok(cycle)
}

/// Only the evaluator's error can propagate from [`recompute_and_reconcile`]
/// when both it and the recompute fail in the same cycle - a single
/// `Result` return has room for one. Logged here, right before that error is
/// returned, so the recompute's own error (a stall's unfinished-partition
/// detail included) still reaches the cron log instead of being silently
/// dropped.
fn log_recompute_error_lost_to_evaluator_failure(
    recomputed: &Result<SpendRecomputeCycle, SpendRecomputeError>,
) {
    if let Err(error) = recomputed {
        tracing::error!(
            error = %error,
            "spend-recompute: the evaluator also failed this cycle, so only its error \
             propagates; logging the recompute's own error here so it is not lost"
        );
    }
}

/// Recompute the current period plus the just-closed previous period while that
/// previous period is still inside the close settle window.
#[allow(clippy::future_not_send)]
pub async fn recompute_unsettled_period_snapshots(
    registry: &Registry,
    stream: &dyn StreamTransport,
    now_unix: i64,
    cfg: &SpendRecomputeConfig,
) -> Result<SpendRecomputeCycle, SpendRecomputeError> {
    let mut total = SpendRecomputeCycle::default();
    for period_start in periods_to_recompute(now_unix, cfg.settle_window) {
        let next = recompute_usage_aggregates(registry, stream, period_start, cfg).await?;
        total.polled += next.polled;
        total.decoded += next.decoded;
        total.skipped_other_period += next.skipped_other_period;
        total.skipped_duplicate += next.skipped_duplicate;
        total.skipped_no_app += next.skipped_no_app;
        total.skipped_zero_value += next.skipped_zero_value;
        total.skipped_undecodable += next.skipped_undecodable;
        total.aggregates += next.aggregates;
        total.written += next.written;
    }
    Ok(total)
}

#[must_use]
pub fn periods_to_recompute(now_unix: i64, settle_window: Duration) -> Vec<i64> {
    let current = period_start_unix(now_unix);
    let previous = super::billing_reconcile::previous_period_start_unix(now_unix);
    if super::billing_reconcile::period_settled(now_unix, previous, settle_window) {
        vec![current]
    } else {
        vec![current, previous]
    }
}

/// Recompute the specified period and overwrite `usage_aggregates`.
///
/// The scan reads the whole retained stream once, up to the ends captured as
/// it starts, folding every in-period event into per-`(app, metric)` totals,
/// and the snapshot is written only once that scan is complete. A scan that
/// stalls, or a stream error partway through, returns its error before
/// anything is written, so the period keeps the totals the last complete scan
/// wrote.
///
/// FINITE BY CONSTRUCTION. Right after `rewind`, [`StreamTransport::end_offsets`]
/// captures the last offset of every partition of the topic. The scan is
/// complete when each partition has delivered a record at or past its captured
/// offset; a partition that held nothing at the capture is complete from the
/// start. Records past a captured end, and records from a partition the capture
/// did not name, belong to the next cycle and are not read, so however fast
/// producers write, the scan has a fixed amount to read.
///
/// BOUNDED BY STALLS, NOT BY SIZE. The only time limit is the transport's
/// [`StreamTransport::stall_timeout`], and it bounds the gap between polls that
/// advance a partition toward its captured end, never the scan as a whole: a
/// period of any size completes as long as the stream keeps delivering. A
/// `poll` that suspends is abandoned once what is left of the current gap runs
/// out (see [`unless_stalled`]), so a poll that parks ends the scan. A `poll`
/// that blocks its thread instead (the redpanda adapter's librdkafka calls)
/// cannot be abandoned, so the stall clock is also checked before every poll;
/// there the gap can overrun by one poll, which that adapter bounds by its own
/// configured poll timeout. A transport with no stall timeout answers every
/// poll at once, so one poll that advances nothing ends the scan there. In each
/// case the scan returns [`SpendRecomputeError::Stalled`] and nothing is
/// written.
///
/// TODO(OQ-7): this intentionally performs a full retained-stream scan each
/// cadence. A later slice should add rolling per-partition slices or an
/// incremental checkpoint/range-scan API to avoid O(period) work every tick.
#[allow(clippy::future_not_send)]
pub async fn recompute_usage_aggregates(
    registry: &Registry,
    stream: &dyn StreamTransport,
    period_start: i64,
    cfg: &SpendRecomputeConfig,
) -> Result<SpendRecomputeCycle, SpendRecomputeError> {
    stream.rewind().await?;
    let ends = stream.end_offsets().await?;
    let mut scan = PeriodScan::new(period_start, &ends);

    let stall = stream.stall_timeout();
    let clock = cfg.clock.as_ref();
    let batch_max = cfg.batch_max.max(1);
    let mut last_advance = clock.now();
    while !scan.is_complete() {
        let records = match stall {
            Some(stall) => {
                let left = stall.saturating_sub(clock.elapsed(last_advance));
                if left.is_zero() {
                    return Err(scan.stalled(stall));
                }
                match unless_stalled(stream.poll(batch_max), compio::time::sleep(left)).await {
                    Some(polled) => polled?,
                    None => return Err(scan.stalled(stall)),
                }
            }
            None => stream.poll(batch_max).await?,
        };
        if scan.fold(&records) {
            last_advance = clock.now();
        } else if stall.is_none() {
            return Err(scan.stalled(Duration::ZERO));
        }
    }
    let PeriodScan {
        mut cycle, totals, ..
    } = scan;

    // A complete scan that found no record leaves the snapshot alone. An empty
    // stream - a topic nothing has published to yet, an in-process transport
    // after a restart - is not evidence that nobody used anything, and
    // replacing the snapshot with an empty set would zero enforcement until
    // the stream refills: an over-limit app would escape its spend limit.
    if cycle.polled == 0 {
        return Ok(cycle);
    }

    let mut aggregates: Vec<_> = totals
        .into_iter()
        .map(|((app_id, metric), total)| UsageAggregate {
            app_id,
            metric,
            total,
        })
        .collect();
    aggregates.sort_by(|a, b| {
        a.app_id
            .as_str()
            .cmp(b.app_id.as_str())
            .then_with(|| a.metric.cmp(&b.metric))
    });
    cycle.aggregates = aggregates.len();
    let write = Metering::new(registry.clone())
        .replace_period_snapshot(period_start, &aggregates)
        .await?;
    cycle.written = write.written;

    // A period total that SHRANK is the only in-band evidence that this scan saw less
    // than the last one did - the stream aged out early-period events before this
    // scan read them. The snapshot is an overwrite, so the smaller number is now what
    // spend enforcement reads: usage appears to fall and apps escape their limits.
    //
    // ERROR, not warn: this is a billing-correctness event that fails OPEN, and it is
    // invisible otherwise - missing early-month events look exactly like no usage early
    // in the month. Reporting only; refusing the write is an operator policy call,
    // because a decrease is also what a legitimate dedup fix or bad-event purge produces.
    for drop in &write.decreased {
        tracing::error!(
            app_id = %drop.app_id.as_str(),
            metric = %drop.metric,
            prior_total = drop.prior_total,
            new_total = drop.new_total,
            billing_event = "usage_total_decreased",
            "spend recompute rewrote a period total DOWNWARD - the source scan saw less \
             than the previous one; spend enforcement now reads the smaller total"
        );
    }
    Ok(cycle)
}

/// One period scan in progress: the running totals and counters, and how far
/// each unfinished partition has been read toward its captured end.
#[derive(Debug)]
struct PeriodScan {
    period_start: i64,
    cycle: SpendRecomputeCycle,
    totals: HashMap<(AppId, String), i64>,
    seen_event_ids: HashSet<String>,
    /// Every partition still short of its captured end, by partition id.
    unfinished: HashMap<i32, PartitionProgress>,
}

#[derive(Debug, Clone, Copy)]
struct PartitionProgress {
    /// The captured offset of the partition's last record: the scan's end.
    last: i64,
    /// The highest offset folded so far, `None` before the first.
    read: Option<i64>,
}

impl PeriodScan {
    fn new(period_start: i64, ends: &[PartitionEnd]) -> Self {
        let unfinished = ends
            .iter()
            .filter_map(|end| {
                end.last_offset
                    .map(|last| (end.partition, PartitionProgress { last, read: None }))
            })
            .collect();
        Self {
            period_start,
            cycle: SpendRecomputeCycle::default(),
            totals: HashMap::new(),
            seen_event_ids: HashSet::new(),
            unfinished,
        }
    }

    fn is_complete(&self) -> bool {
        self.unfinished.is_empty()
    }

    fn stalled(&self, stall: Duration) -> SpendRecomputeError {
        let mut unfinished: Vec<UnfinishedPartition> = self
            .unfinished
            .iter()
            .map(|(&partition, progress)| UnfinishedPartition {
                partition,
                captured_end: progress.last,
                last_read: progress.read,
            })
            .collect();
        unfinished.sort_unstable_by_key(|partition| partition.partition);
        SpendRecomputeError::Stalled {
            stall_ms: u64::try_from(stall.as_millis()).unwrap_or(u64::MAX),
            unfinished,
        }
    }

    /// Fold one polled batch. A record advances its partition when it lies
    /// past the highest offset read there so far; one at or past the captured
    /// end finishes the partition, and only records up to that end are read
    /// into the totals. Records of a finished partition, or of one the capture
    /// did not name, are passed over. Returns whether any record advanced.
    ///
    /// Finishing on a record PAST the end, not only at it, keeps a partition
    /// whose captured last offset is never delivered - a transaction marker,
    /// say - from holding the scan open once anything newer arrives.
    fn fold(&mut self, records: &[StreamRecord]) -> bool {
        let mut advanced = false;
        for record in records {
            let Some(progress) = self.unfinished.get_mut(&record.partition) else {
                continue;
            };
            if progress.read.is_some_and(|read| record.offset <= read) {
                continue;
            }
            progress.read = Some(record.offset);
            let within_end = record.offset <= progress.last;
            if record.offset >= progress.last {
                self.unfinished.remove(&record.partition);
            }
            advanced = true;
            if within_end {
                self.read(record);
            }
        }
        advanced
    }

    /// Decode one in-range record and apply it to the totals and counters.
    fn read(&mut self, record: &StreamRecord) {
        self.cycle.polled += 1;
        let event = match decode_record(record) {
            Ok(event) => event,
            Err(error) => {
                self.cycle.skipped_undecodable += 1;
                tracing::warn!(
                    partition = record.partition,
                    offset = record.offset,
                    error = %error,
                    "spend_recompute: skipping undecodable usage stream record"
                );
                return;
            }
        };
        self.cycle.decoded += 1;
        if !self.seen_event_ids.insert(event.event_id.clone()) {
            self.cycle.skipped_duplicate += 1;
            return;
        }
        match apply_event(&mut self.totals, &event, self.period_start) {
            Applied::Yes => {}
            Applied::OtherPeriod => self.cycle.skipped_other_period += 1,
            Applied::NoAppSubject => self.cycle.skipped_no_app += 1,
            Applied::ZeroValue => self.cycle.skipped_zero_value += 1,
        }
    }
}

fn apply_event(
    totals: &mut HashMap<(AppId, String), i64>,
    event: &UsageEvent,
    period_start: i64,
) -> Applied {
    if period_start_unix(event.event_time) != period_start {
        return Applied::OtherPeriod;
    }
    let Some(app_id) = event.subject.app.clone() else {
        tracing::warn!(
            event_id = %event.event_id,
            meter = %event.meter,
            "spend_recompute: usage event has no app subject — skipping"
        );
        return Applied::NoAppSubject;
    };
    let value = match i64::try_from(event.value) {
        Ok(value) => value,
        Err(_) => {
            tracing::warn!(
                event_id = %event.event_id,
                meter = %event.meter,
                value = event.value,
                "spend_recompute: usage event value exceeds i64::MAX — saturating for conservative enforcement"
            );
            i64::MAX
        }
    };
    if value == 0 {
        return Applied::ZeroValue;
    }

    let total = totals.entry((app_id.clone(), event.meter.clone())).or_insert(0);
    match total.checked_add(value) {
        Some(sum) => *total = sum,
        None => {
            *total = i64::MAX;
            tracing::warn!(
                app_id = %app_id.as_str(),
                meter = %event.meter,
                "spend_recompute: aggregate total overflow — saturating to i64::MAX for conservative enforcement"
            );
        }
    }
    Applied::Yes
}

fn decode_record(record: &StreamRecord) -> Result<UsageEvent, SpendRecomputeError> {
    serde_json::from_slice(&record.payload).map_err(|source| SpendRecomputeError::Decode {
        partition: record.partition,
        offset: record.offset,
        source,
    })
}

/// Database-free unit tests. Kept in their own module so they stay visible to a
/// bare `cargo test --workspace`, which provisions no PostgreSQL: everything in
/// `live_db_tests` below opens a real connection and `expect`s it.
#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn periods_to_recompute_include_previous_until_settle_window_closes() {
        let now = chrono::Utc
            .with_ymd_and_hms(2035, 7, 1, 0, 10, 0)
            .unwrap()
            .timestamp();
        let current = period_start_unix(now);
        let previous = super::super::billing_reconcile::previous_period_start_unix(now);
        assert_eq!(
            periods_to_recompute(now, Duration::from_secs(3600)),
            vec![current, previous],
            "just-closed previous period stays in the witness recompute while settling"
        );

        let settled = super::super::billing_reconcile::period_end_unix(previous) + 3601;
        assert_eq!(
            periods_to_recompute(settled, Duration::from_secs(3600)),
            vec![period_start_unix(settled)],
            "after the settle window closes only the current period is recomputed"
        );
    }

    /// A usage event whose `event_time` belongs to another period is dropped,
    /// however faithfully it was retained and replayed.
    ///
    /// This is the ENFORCEMENT half of the late-arrival gap. The producer's WAL
    /// republishes an unpublished event verbatim, keeping its original
    /// `event_time` - correct, and what makes publishing idempotent. But a
    /// period leaves `periods_to_recompute` once it settles, so an event landing
    /// after that is not counted here, and an app that overspent during a long
    /// broker outage is never throttled for it.
    ///
    /// It is NOT the billing half, and the distinction is worth stating because
    /// getting it backwards costs revenue: the provider forwarder has no age or
    /// period gate at all, so the same late event still becomes an invoice item
    /// with its true timestamp. Late events lose their enforcement value and
    /// keep their billing value - expiring them on a staleness rule would throw
    /// away money that would otherwise be invoiced.
    ///
    /// That billing half is a HOLE, not a handoff: nothing asserts it anywhere.
    /// `event_forwarder` has no inline test module, and every event in
    /// `tests/stream_forwarder_recompute_test.rs` is stamped in-period
    /// (`period + 10/20/30`), so no test drives a late event through the
    /// forwarder to see it billed. Said plainly because an exclusion that does
    /// not name where the class IS covered leaves a reader unable to tell a gap
    /// from a delegation.
    ///
    /// Every other test here uses an in-period timestamp, so this branch had no
    /// coverage.
    #[test]
    fn a_usage_event_from_another_period_is_not_counted() {
        let app = AppId::mint();
        let organization = zeroship_core::typed_id::generate("org");
        let period = period_start_unix(1_783_468_800);
        let previous = period_start_unix(period - 1);
        assert_ne!(period, previous, "the two stamps must be in different periods");

        let mk = |id: &str, value: u64, event_time: i64| zeroship_core::usage_event::UsageEvent {
            event_id: id.to_string(),
            source: "worker-test".to_string(),
            subject: zeroship_core::usage_event::UsageSubject {
                app: Some(app.clone()),
                organization: Some(organization.clone()),
            },
            meter: "requests".to_string(),
            value,
            event_time,
            dims: std::collections::BTreeMap::new(),
        };

        let mut totals = HashMap::new();

        assert_eq!(
            apply_event(&mut totals, &mk("evt_in", 100, period + 10), period),
            Applied::Yes,
            "an in-period event must be counted"
        );
        assert_eq!(totals.get(&(app.clone(), "requests".to_string())), Some(&100));

        // Asserts the REASON, not just the refusal: a late retry that lost its
        // enforcement value must be distinguishable from an event with no app
        // subject or a zero value.
        assert_eq!(
            apply_event(&mut totals, &mk("evt_late", 5_000, previous + 10), period),
            Applied::OtherPeriod,
            "an event from a settled period must not be counted, and must say why"
        );
        assert_eq!(
            totals.get(&(app, "requests".to_string())),
            Some(&100),
            "a dropped event must leave the totals untouched"
        );
    }
}

/// The stream-recompute enforcement regression tests (event_id dedup, poison
/// skip, empty-cycle snapshot preservation, unsettled-period rewrite). Each one
/// seeds `zeroship.apps` / pricing rows and reads back the persisted spend
/// snapshot, so it needs a reachable, migrated PostgreSQL and panics on connect
/// without one.
///
/// Included in ordinary cargo test with a crate-owned PostgreSQL fixture.
#[cfg(test)]
mod live_db_tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Instant;

    use chrono::TimeZone;
    use compio_postgres::{connect, NoTls};
    use uuid::Uuid;
    use zeroship_core::types::SpendState;
    use zeroship_core::usage_event::UsageSubject;
    use zeroship_stream::{Clock, StreamError, StreamOffset};

    use crate::metering::{current_period_start_unix, period_date};
    use crate::spend::{parse_spend_state, SpendEngine};

    use super::*;

    #[derive(Debug)]
    struct FakeStream {
        records: Vec<StreamRecord>,
        cursor: Mutex<usize>,
    }

    #[async_trait::async_trait(?Send)]
    impl StreamTransport for FakeStream {
        fn id(&self) -> &str {
            "fake-retained"
        }

        async fn publish(
            &self,
            _topic: &str,
            _partition_key: &[u8],
            _payload: &[u8],
        ) -> Result<(), StreamError> {
            Ok(())
        }

        async fn poll(&self, max: usize) -> Result<Vec<StreamRecord>, StreamError> {
            let mut cursor = self.cursor.lock().expect("fake stream cursor poisoned");
            let start = *cursor;
            let end = (start + max).min(self.records.len());
            *cursor = end;
            Ok(self.records[start..end].to_vec())
        }

        async fn commit(&self, _offsets: &[StreamOffset]) -> Result<(), StreamError> {
            Ok(())
        }

        async fn rewind(&self) -> Result<(), StreamError> {
            *self.cursor.lock().expect("fake stream cursor poisoned") = 0;
            Ok(())
        }

        async fn end_offsets(&self) -> Result<Vec<PartitionEnd>, StreamError> {
            Ok(ends_of(&self.records))
        }

        /// An in-process log: every poll hands over what it holds at once.
        fn stall_timeout(&self) -> Option<Duration> {
            None
        }
    }

    /// The end of every partition `records` touches: its highest offset.
    fn ends_of(records: &[StreamRecord]) -> Vec<PartitionEnd> {
        let mut last = BTreeMap::<i32, i64>::new();
        for record in records {
            let entry = last.entry(record.partition).or_insert(record.offset);
            *entry = (*entry).max(record.offset);
        }
        last.into_iter()
            .map(|(partition, last)| PartitionEnd {
                partition,
                last_offset: Some(last),
            })
            .collect()
    }

    /// A clone of the migrated template this case owns.
    ///
    /// A recompute cycle deletes and rewrites every usage aggregate of a
    /// period, so a case works in a clone: on the shared database its rewrite of
    /// the current month would delete a sibling case's rows. The case holds the
    /// clone as a local, so it is dropped when the case returns, on the case's
    /// own thread. A thread-local would drop it during that thread's teardown,
    /// and the drop enters the testkit's daemon runtime through tokio's
    /// thread-local context, which teardown may already have destroyed; a
    /// process-wide static would keep every clone until the shared server goes.
    fn fresh_database() -> zeroship_testkit::postgres::FreshDatabase {
        zeroship_testkit::postgres::platform().fresh_database()
    }

    async fn pg(db_url: &str) -> compio_postgres::Client {
        let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        client
    }

    async fn seed_priced_app(
        client: &compio_postgres::Client,
        plan_id: &str,
        label: &str,
    ) -> AppId {
        let app_id = AppId::mint();
        let name = format!("{label}-{}", Uuid::new_v4());
        // A LIVE app needs a project and a project needs an organization:
        // `apps_live_app_has_project` (`project_id IS NOT NULL OR deleted_at IS
        // NOT NULL`) against a RESTRICT foreign key. Only a deleted app may
        // detach, and this fixture seeds live apps, so the project is still
        // mandatory here. The
        // organization here has no members, because recompute is a fleet-wide
        // sweep keyed on the app and this fixture asserts nothing about
        // authority.
        let organization_id = zeroship_core::typed_id::generate("org");
        let project_id = zeroship_core::typed_id::generate("prj");
        client
            .execute(
                "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
                 VALUES ($1, $2, 'Recompute Fixture', 'fixture@zeroship.test')",
                &[&organization_id, &format!("recompute-{}", Uuid::new_v4().simple())],
            )
            .await
            .expect("seed fixture organization");
        client
            .execute(
                "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
                 VALUES ($1, $2, 'default', 'Default')",
                &[&project_id, &organization_id],
            )
            .await
            .expect("seed fixture project");
        client
            .execute(
                "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
                 SELECT $1, $2, $3, p.id, p.organization_id FROM zeroship.projects p \
                  WHERE p.id = $4",
                &[&app_id.as_str(), &name, &plan_id, &project_id],
            )
            .await
            .expect("insert app");
        app_id
    }

    async fn seed_pricing(client: &compio_postgres::Client) -> String {
        client
            .execute(
                "INSERT INTO zeroship.billing_metrics (metric, kind, unit) \
                 VALUES ('requests', 'platform', 'op') \
                 ON CONFLICT (metric) DO UPDATE SET unit = EXCLUDED.unit",
                &[],
            )
            .await
            .expect("seed requests metric");
        client
            .execute(
                "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
                 VALUES ('requests', 1, 1) \
                 ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
                &[],
            )
            .await
            .expect("seed requests weight");
        // The global default FX must EXIST, even though this test's own plan
        // carries an explicit fx: `evaluate_all` sweeps every app in the
        // database, and any app on a plan with a NULL fx inherits the global.
        // Missing it aborts the whole sweep rather than billing an unpriceable
        // $0, so the sweep fails for reasons that have nothing to do with this
        // test's apps.
        //
        // It is seeded by migration 0041, but `billing_reconcile_test` DELETEs
        // the singleton to prove the fail-closed path and restores it a few
        // statements later; a panic in between (there are `.expect`s in that
        // window) leaks the deletion into the shared database and, because the
        // database outlives a single `cargo test`, into LATER RUNS. That is what
        // made this test fail, pass, then fail again on identical code. Seeding
        // it here makes the precondition this test's own business instead of a
        // wager on what ran before it.
        client
            .execute(
                "INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit) \
                 VALUES ('global', 30000000) ON CONFLICT (id) DO NOTHING",
                &[],
            )
            .await
            .expect("seed global default fx");
        let plan_id = format!("pln_recompute_{}", Uuid::new_v4().simple());
        let fx_one_cent: i64 = 1_000_000_000_000;
        client
            .execute(
                "INSERT INTO zeroship.plans \
                   (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                    runtime_limits_json, heap_limit_mb, spend_limit_default_cents) \
                 VALUES ($1, 'recompute-test', 0, 0, $2, \
                         '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000}', 64, 100)",
                &[&plan_id, &fx_one_cent],
            )
            .await
            .expect("seed recompute plan");
        plan_id
    }

    #[compio::test]
    async fn stream_recompute_replaces_usage_aggregates_and_spend_state_idempotently() {
        // `evaluate_all` below sweeps every app fleet-wide, so this case works
        // in a clone of the migrated template: a sibling's sweep must not
        // transition this case's apps first and leave this case's `evaluate_all`
        // with nothing to report.
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let period = current_period_start_unix();
        let plan_id = seed_pricing(&client).await;
        let warn_app = seed_priced_app(&client, &plan_id, "recompute-warn").await;
        let degrade_app = seed_priced_app(&client, &plan_id, "recompute-degrade").await;
        let block_app = seed_priced_app(&client, &plan_id, "recompute-block").await;
        let organization = zeroship_core::typed_id::generate("org");
        let stream = FakeStream::new(vec![
            event("evt_warn_a", &warn_app, &organization, 30, period + 10),
            event("evt_warn_b", &warn_app, &organization, 50, period + 11),
            event("evt_degrade", &degrade_app, &organization, 95, period + 12),
            event("evt_block", &block_app, &organization, 100, period + 13),
            event("evt_old_period", &block_app, &organization, 999, period.saturating_sub(60)),
        ]);
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(
                super::super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: 2,
            ..Default::default()
        };

        let first = recompute_usage_aggregates(&registry, &stream, period, &cfg)
            .await
            .expect("first recompute");
        assert_eq!(first.polled, 5);
        assert_eq!(first.decoded, 5);
        // evt_old_period: right app, wrong period. Now named, not lumped.
        assert_eq!(first.skipped_other_period, 1);
        assert_eq!(first.skipped_duplicate, 0);
        assert_eq!(first.skipped_no_app, 0);
        assert_eq!(first.aggregates, 3);
        assert_eq!(first.written, 3);
        assert_total(&client, &warn_app, period, 80).await;
        assert_total(&client, &degrade_app, period, 95).await;
        assert_total(&client, &block_app, period, 100).await;

        let second = recompute_usage_aggregates(&registry, &stream, period, &cfg)
            .await
            .expect("second recompute");
        assert_eq!(second.written, 3);
        assert_total(&client, &warn_app, period, 80).await;
        assert_total(&client, &degrade_app, period, 95).await;
        assert_total(&client, &block_app, period, 100).await;

        let transitions = SpendEngine::new(registry)
            .evaluate_all()
            .await
            .expect("evaluate_all");
        assert!(transitions.iter().any(|t| t.app_id == warn_app));
        assert!(transitions.iter().any(|t| t.app_id == degrade_app));
        assert!(transitions.iter().any(|t| t.app_id == block_app));
        assert_state(&client, &warn_app, SpendState::Warn).await;
        assert_state(&client, &degrade_app, SpendState::Degrade).await;
        assert_state(&client, &block_app, SpendState::Block).await;
    }

    #[compio::test]
    async fn stream_recompute_empty_cycle_does_not_wipe_snapshot() {
        // A complete scan of a stream holding no record (a topic nothing has
        // published to yet, an in-process transport after a restart) must leave
        // the last good snapshot in place, not overwrite it with zero.
        // Otherwise enforcement drops to "no usage" and an over-limit app
        // escapes its spend limit until the stream refills.
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let period = current_period_start_unix();
        let plan_id = seed_pricing(&client).await;
        let app = seed_priced_app(&client, &plan_id, "recompute-nowipe").await;
        let organization = zeroship_core::typed_id::generate("org");
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(
                super::super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: 8,
            ..Default::default()
        };

        // Cycle 1: real usage lands.
        let populated = FakeStream::new(vec![event("evt_real", &app, &organization, 100, period + 10)]);
        let first = recompute_usage_aggregates(&registry, &populated, period, &cfg)
            .await
            .expect("populated recompute");
        assert_eq!(first.written, 1);
        assert_total(&client, &app, period, 100).await;

        // Cycle 2: an EMPTY stream must NOT wipe the snapshot.
        let empty = FakeStream::new(vec![]);
        let second = recompute_usage_aggregates(&registry, &empty, period, &cfg)
            .await
            .expect("empty recompute");
        assert_eq!(second.polled, 0);
        assert_eq!(second.written, 0, "an empty cycle writes nothing");
        assert_total(&client, &app, period, 100).await; // snapshot preserved
    }

    #[compio::test]
    async fn stream_recompute_dedups_duplicate_event_ids() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let period = current_period_start_unix();
        let plan_id = seed_pricing(&client).await;
        let app = seed_priced_app(&client, &plan_id, "recompute-dedup").await;
        let organization = zeroship_core::typed_id::generate("org");
        let stream = FakeStream::new(vec![
            event("evt_duplicate_replay", &app, &organization, 40, period + 10),
            event("evt_duplicate_replay", &app, &organization, 40, period + 10),
            event("evt_distinct", &app, &organization, 2, period + 11),
        ]);
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(
                super::super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: 2,
            ..Default::default()
        };

        let cycle = recompute_usage_aggregates(&registry, &stream, period, &cfg)
            .await
            .expect("recompute with duplicate event_id");
        assert_eq!(cycle.polled, 3);
        assert_eq!(cycle.decoded, 3);
        // evt_duplicate_replay: same event_id twice. This is the counter for a
        // duplicate `event_id`, distinct from a wrong-period event.
        assert_eq!(cycle.skipped_duplicate, 1);
        assert_eq!(cycle.skipped_other_period, 0);
        assert_eq!(cycle.aggregates, 1);
        assert_eq!(cycle.written, 1);
        assert_total(&client, &app, period, 42).await;

        // The clone is this case's own: once the case lets it go it is gone,
        // rather than kept for the process.
        let name = fresh.admin_url().path().trim_start_matches('/').to_owned();
        drop(registry);
        drop(client);
        drop(fresh);
        let admin = pg(zeroship_testkit::postgres::platform().admin_url().as_str()).await;
        let left: i64 = admin
            .query_one("SELECT count(*) FROM pg_database WHERE datname = $1", &[&name])
            .await
            .expect("count the case's clone")
            .get(0);
        assert_eq!(left, 0, "the case's clone {name} outlived the case");
    }

    #[compio::test]
    async fn stream_recompute_skips_undecodable_records() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let period = current_period_start_unix();
        let plan_id = seed_pricing(&client).await;
        let app = seed_priced_app(&client, &plan_id, "recompute-poison").await;
        let organization = zeroship_core::typed_id::generate("org");
        let stream = FakeStream::from_records(vec![
            record_from_event(0, event("evt_before_poison", &app, &organization, 10, period + 10)),
            StreamRecord {
                partition: 0,
                offset: 1,
                key: b"poison".to_vec(),
                payload: b"{not-json".to_vec(),
            },
            record_from_event(2, event("evt_after_poison", &app, &organization, 7, period + 11)),
        ]);
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(
                super::super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: 2,
            ..Default::default()
        };

        let cycle = recompute_usage_aggregates(&registry, &stream, period, &cfg)
            .await
            .expect("recompute skips undecodable stream records");
        assert_eq!(cycle.polled, 3);
        assert_eq!(cycle.decoded, 2);
        assert_eq!(cycle.skipped_other_period, 0);
        assert_eq!(cycle.skipped_duplicate, 0);
        assert_eq!(cycle.skipped_undecodable, 1);
        assert_eq!(cycle.aggregates, 1);
        assert_eq!(cycle.written, 1);
        assert_total(&client, &app, period, 17).await;
    }

    #[compio::test]
    async fn unsettled_period_recompute_rewrites_current_and_previous_snapshots() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let now = chrono::Utc
            .with_ymd_and_hms(2036, 8, 1, 0, 10, 0)
            .unwrap()
            .timestamp();
        let current = period_start_unix(now);
        let previous = super::super::billing_reconcile::previous_period_start_unix(now);
        let plan_id = seed_pricing(&client).await;
        let app = seed_priced_app(&client, &plan_id, "recompute-prev").await;
        let organization = zeroship_core::typed_id::generate("org");
        let stream = FakeStream::new(vec![
            event("evt_prev_unsettled", &app, &organization, 41, previous + 10),
            event("evt_current_unsettled", &app, &organization, 59, current + 10),
        ]);
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(3600),
            batch_max: 1,
            ..Default::default()
        };

        let cycle = recompute_unsettled_period_snapshots(&registry, &stream, now, &cfg)
            .await
            .expect("recompute unsettled periods");
        assert_eq!(
            cycle.polled, 4,
            "each period recompute scans the retained stream"
        );
        assert_eq!(cycle.decoded, 4);
        // Two periods scanned, each seeing the other period+s event: the
        // dominant-by-construction case the split exists to isolate.
        assert_eq!(cycle.skipped_other_period, 2);
        assert_eq!(cycle.skipped_duplicate, 0);
        assert_eq!(cycle.aggregates, 2);
        assert_eq!(cycle.written, 2);
        assert_total(&client, &app, previous, 41).await;
        assert_total(&client, &app, current, 59).await;
    }

    /// Hand control back to the runtime once. Every scripted stream below
    /// suspends in `poll`, the way a real fetch does, so the test's own timeout
    /// can always fire: a scan that lost its guard then fails by name inside
    /// the test's budget instead of spinning until the test runner kills it.
    async fn yield_to_runtime() {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                std::task::Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await;
    }

    /// A clock a test advances itself, so pacing is exact and immune to real
    /// scheduler latency: nothing here ever calls `compio::time::sleep` or
    /// otherwise waits on a real timer to simulate a gap.
    #[derive(Debug)]
    struct FakeClock {
        now: Mutex<Instant>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Mutex::new(Instant::now()),
            }
        }

        fn advance(&self, by: Duration) {
            let mut now = self.now.lock().expect("fake clock poisoned");
            *now += by;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self.now.lock().expect("fake clock poisoned")
        }
    }

    /// What a [`ScriptedStream`]'s `poll` does on each call.
    #[derive(Debug, Clone, Copy)]
    enum Script {
        /// Hand over the next records: a healthy broker.
        Delivers,
        /// Advance [`ScriptedStream::clock`] by this much, then hand over the
        /// next records: a slow broker that keeps delivering. Simulated, not
        /// slept, so a caller reading elapsed time from that same clock sees
        /// an exact, reproducible gap.
        Paced(Duration),
        /// Append one record to the log, then hand over the next records: a
        /// producer writing while the scan reads.
        Growing,
        /// Never answer: a fetch the broker accepted and went silent on.
        Parked,
        /// Hand over the first this-many records, then answer with nothing: a
        /// partition that stops delivering short of its end.
        StopsAfter(usize),
        /// Hand over the first this-many records, then hand the last of them
        /// over again on every call: a broker replaying an old offset.
        ReplaysAfter(usize),
        /// Like `StopsAfter`, but answering at once without ever suspending,
        /// the way an adapter whose `poll` blocks its thread answers (the
        /// redpanda adapter's librdkafka calls). A test's own timeout cannot
        /// fire around a loop over such a poll, so the stream is its own
        /// watchdog: polled long past its stall timeout, it panics by name.
        Blocking(usize),
    }

    /// A stream whose log the test writes and whose `poll` follows a
    /// [`Script`]. `end_offsets` reports the log as it stands at the call,
    /// like a broker's watermark, unless the test overrides it in `ends`;
    /// `stall_timeout` is the test's.
    #[derive(Debug)]
    struct ScriptedStream {
        log: Mutex<Vec<StreamRecord>>,
        cursor: Mutex<usize>,
        script: Script,
        stall: Option<Duration>,
        /// Builds the record a `Growing` script appends at an offset.
        appended: fn(i64) -> StreamRecord,
        /// What `end_offsets` reports instead of the log's own ends.
        ends: Option<Vec<PartitionEnd>>,
        created: Instant,
        /// Advanced by [`Script::Paced`] on every simulated delivery. Unused
        /// by every other script. A test that wants a [`SpendRecomputeConfig`]
        /// to read elapsed time from this SAME clock retrieves it via
        /// [`Self::fake_clock`] and passes it to
        /// [`SpendRecomputeConfig::with_clock`].
        clock: Arc<FakeClock>,
    }

    impl ScriptedStream {
        fn new(records: Vec<StreamRecord>, script: Script, stall: Option<Duration>) -> Self {
            Self {
                log: Mutex::new(records),
                cursor: Mutex::new(0),
                script,
                stall,
                appended: |offset| StreamRecord {
                    partition: 0,
                    offset,
                    key: Vec::new(),
                    payload: b"{appended after the capture".to_vec(),
                },
                ends: None,
                created: Instant::now(),
                clock: Arc::new(FakeClock::new()),
            }
        }

        fn fake_clock(&self) -> Arc<FakeClock> {
            Arc::clone(&self.clock)
        }

        fn next(&self, max: usize, limit: usize) -> Vec<StreamRecord> {
            let log = self.log.lock().expect("scripted log poisoned");
            let (start, end) = {
                let mut cursor = self.cursor.lock().expect("scripted cursor poisoned");
                let start = *cursor;
                *cursor = (start + max).min(limit).min(log.len());
                (start, *cursor)
            };
            log[start..end].to_vec()
        }
    }

    #[async_trait::async_trait(?Send)]
    impl StreamTransport for ScriptedStream {
        fn id(&self) -> &'static str {
            "scripted"
        }

        async fn publish(
            &self,
            _topic: &str,
            _partition_key: &[u8],
            _payload: &[u8],
        ) -> Result<(), StreamError> {
            Ok(())
        }

        async fn poll(&self, max: usize) -> Result<Vec<StreamRecord>, StreamError> {
            match self.script {
                Script::Delivers => {
                    yield_to_runtime().await;
                    Ok(self.next(max, usize::MAX))
                }
                Script::Paced(pace) => {
                    self.clock.advance(pace);
                    Ok(self.next(max, usize::MAX))
                }
                Script::Growing => {
                    yield_to_runtime().await;
                    {
                        let mut log = self.log.lock().expect("scripted log poisoned");
                        let offset = i64::try_from(log.len()).expect("offset fits i64");
                        log.push((self.appended)(offset));
                    }
                    Ok(self.next(max, usize::MAX))
                }
                Script::Parked => std::future::pending().await,
                Script::StopsAfter(count) => {
                    yield_to_runtime().await;
                    Ok(self.next(max, count))
                }
                Script::ReplaysAfter(count) => {
                    yield_to_runtime().await;
                    let next = self.next(max, count);
                    if next.is_empty() {
                        let log = self.log.lock().expect("scripted log poisoned");
                        return Ok(vec![log[count - 1].clone()]);
                    }
                    Ok(next)
                }
                Script::Blocking(count) => {
                    let watchdog = self.stall.expect("a blocking script has a stall timeout") * 25;
                    assert!(
                        self.created.elapsed() < watchdog,
                        "the scan kept polling a stream that answers at once without advancing \
                         long past its stall timeout"
                    );
                    Ok(self.next(max, count))
                }
            }
        }

        async fn commit(&self, _offsets: &[StreamOffset]) -> Result<(), StreamError> {
            Ok(())
        }

        async fn rewind(&self) -> Result<(), StreamError> {
            *self.cursor.lock().expect("scripted cursor poisoned") = 0;
            Ok(())
        }

        async fn end_offsets(&self) -> Result<Vec<PartitionEnd>, StreamError> {
            Ok(self
                .ends
                .clone()
                .unwrap_or_else(|| ends_of(&self.log.lock().expect("scripted log poisoned"))))
        }

        fn stall_timeout(&self) -> Option<Duration> {
            self.stall
        }
    }

    fn scan_cfg(batch_max: usize) -> SpendRecomputeConfig {
        SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(
                super::super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max,
            ..Default::default()
        }
    }

    /// One priced app with one in-period usage record per value, and those
    /// records' total already stored as the period's snapshot, so a later scan
    /// that must not write can be seen not to have. The snapshot is seeded
    /// with SQL rather than by a scan, so the starting state does not depend
    /// on the code under test.
    struct Snapshotted {
        app: AppId,
        period: i64,
        records: Vec<StreamRecord>,
        total: i64,
    }

    async fn snapshotted(client: &compio_postgres::Client, values: &[u64]) -> Snapshotted {
        let period = current_period_start_unix();
        let plan_id = seed_pricing(client).await;
        let app = seed_priced_app(client, &plan_id, "recompute-scan").await;
        let organization = zeroship_core::typed_id::generate("org");
        let records: Vec<StreamRecord> = values
            .iter()
            .enumerate()
            .map(|(offset, value)| {
                let at = i64::try_from(offset).expect("offset fits i64");
                record_from_event(
                    at,
                    event(&format!("evt_scan_{at}"), &app, &organization, *value, period + 10 + at),
                )
            })
            .collect();
        let total: i64 = values
            .iter()
            .map(|value| i64::try_from(*value).expect("value fits i64"))
            .sum();
        client
            .execute(
                "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total, updated_at) \
                 VALUES ($1, $2::date, 'requests', $3, NOW())",
                &[&app.as_str(), &period_date(period), &total],
            )
            .await
            .expect("seed the starting snapshot");
        assert_total(client, &app, period, total).await;
        Snapshotted {
            app,
            period,
            records,
            total,
        }
    }

    /// THE STARVATION CASE. A period whose scan takes longer than one stall
    /// timeout, while the stream keeps delivering, completes and writes its
    /// snapshot. The stall timeout bounds the gap between records that
    /// advance, not the scan, so a large backlog is never cut off.
    ///
    /// Deterministic by construction: `Script::Paced` advances a [`FakeClock`]
    /// by `pace` per delivered batch, and the scan reads elapsed time from
    /// that SAME clock via [`SpendRecomputeConfig::with_clock`] - never a
    /// real timer - so the scan's simulated total can exceed one stall
    /// timeout with no dependence on how promptly this process actually gets
    /// scheduled. A version paced with a real `compio::time::sleep` per batch
    /// flakes under load: a 200ms real timer can fire well after 200ms when
    /// the host is busy, and a single overrun gap past the 1s stall then
    /// looks exactly like a stalled broker.
    ///
    /// WHAT IT CATCHES: measuring the timeout from the start of the scan
    /// instead of from the last advance. Then every cycle of a period this
    /// large would stall at the same point, write nothing, and start over, and
    /// enforcement would never move.
    #[compio::test]
    async fn a_backlog_longer_than_one_stall_timeout_completes_while_it_advances() {
        let values: [u64; 8] = [10, 20, 30, 40, 50, 60, 70, 80];
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let base = snapshotted(&client, &values[..1]).await;
        let stall = Duration::from_secs(1);
        let pace = stall / 5;
        assert!(
            pace * u32::try_from(values.len()).expect("small backlog") > stall,
            "the scan must outlast one stall timeout or this proves nothing"
        );
        let organization = zeroship_core::typed_id::generate("org");
        let records: Vec<StreamRecord> = values
            .iter()
            .enumerate()
            .map(|(offset, value)| {
                let at = i64::try_from(offset).expect("offset fits i64");
                record_from_event(
                    at,
                    event(&format!("evt_backlog_{at}"), &base.app, &organization, *value, base.period + at),
                )
            })
            .collect();
        let stream = ScriptedStream::new(records, Script::Paced(pace), Some(stall));
        let cfg = scan_cfg(1).with_clock(stream.fake_clock());

        let cycle = recompute_usage_aggregates(&registry, &stream, base.period, &cfg)
            .await
            .expect("a scan that keeps advancing must complete, however long it takes");
        assert_eq!(cycle.polled, values.len());
        assert_eq!(cycle.written, 1);
        let expected = i64::try_from(values.iter().sum::<u64>()).expect("total fits i64");
        assert_total(&client, &base.app, base.period, expected).await;
    }

    /// A producer appending while the scan reads cannot stretch it: the scan
    /// ends at the offsets captured when it started and reads exactly the
    /// records up to them. What is appended meanwhile belongs to the next
    /// cycle.
    ///
    /// WHAT IT CATCHES: ending the scan on anything but the captured ends.
    /// Waiting for the stream to fall idle never ends against this producer
    /// (the test's own timeout fails it), and reading past the captured end
    /// counts the appended records, which the exact totals refuse.
    #[compio::test]
    async fn a_scan_ends_at_the_offsets_captured_when_it_started() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let base = snapshotted(&client, &[5, 7, 11]).await;
        let stall = Duration::from_secs(1);
        let mut stream = ScriptedStream::new(base.records.clone(), Script::Growing, Some(stall));
        stream.appended = |offset| {
            let app = AppId::mint();
            record_from_event(
                offset,
                event(
                    &format!("evt_appended_{offset}"),
                    &app,
                    "org_appended",
                    1_000,
                    current_period_start_unix() + 10,
                ),
            )
        };

        let cycle = compio::time::timeout(
            stall * 25,
            Box::pin(recompute_usage_aggregates(&registry, &stream, base.period, &scan_cfg(2))),
        )
        .await
        .expect("the scan did not return within its test budget: it chased the producer")
        .expect("a scan against a live producer completes at its captured ends");
        let appended = stream.log.lock().expect("scripted log poisoned").len() - base.records.len();
        assert!(appended > 0, "the producer appended while the scan read, or this proves nothing");
        assert_eq!(cycle.polled, base.records.len(), "exactly the captured records are read");
        assert_eq!(cycle.aggregates, 1, "no appended record reached the totals: {cycle:?}");
        assert_eq!(cycle.written, 1, "the complete scan wrote its snapshot");
        assert_total(&client, &base.app, base.period, base.total).await;
    }

    /// A poll that parks is a stall: the scan gives up once the stall timeout
    /// has passed without an advance, returns the named error, and writes
    /// nothing, so the snapshot already stored stays in force.
    ///
    /// WHAT IT CATCHES: removing the bound around `poll`. The parked poll then
    /// never returns and the test's own timeout fails it by name.
    #[compio::test]
    async fn a_poll_that_parks_is_a_stall_and_writes_nothing() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let base = snapshotted(&client, &[100, 50]).await;
        let stall = Duration::from_millis(200);
        let stream = ScriptedStream::new(base.records.clone(), Script::Parked, Some(stall));

        let started = Instant::now();
        let outcome = compio::time::timeout(
            stall * 25,
            Box::pin(recompute_usage_aggregates(&registry, &stream, base.period, &scan_cfg(8))),
        )
        .await
        .expect("the scan did not return within its test budget: a parked poll held it open");
        match outcome {
            Err(SpendRecomputeError::Stalled { stall_ms, unfinished }) => {
                assert_eq!(stall_ms, 200, "the error names the transport's stall timeout");
                assert_eq!(unfinished.len(), 1);
            }
            other => panic!("expected the parked scan to stall, got {other:?}"),
        }
        assert!(
            started.elapsed() >= stall,
            "the scan gave up before the stall timeout had passed"
        );
        assert_total(&client, &base.app, base.period, base.total).await;
    }

    /// A scan that stops advancing partway writes nothing. Here one partition
    /// is read to its end and the other delivers its first record and then
    /// nothing, the shape of a fetch that stalls mid-scan or a partition this
    /// consumer is never handed. The partial totals would undercount; the
    /// snapshot already stored must survive instead.
    ///
    /// WHAT IT CATCHES: writing whatever a scan saw once the stream goes
    /// quiet, which is what ending on idle polls does, and writing anything
    /// at all from a scan that stalled.
    ///
    /// WHAT IT DOES NOT CATCH: removing the check of the stall clock between
    /// polls. This stream's polls suspend, so the bound around each poll ends
    /// the scan anyway; the blocking case below is the one that needs the
    /// check.
    #[compio::test]
    async fn a_scan_that_stalls_partway_writes_nothing() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let base = snapshotted(&client, &[100, 50, 25]).await;
        let mut records = base.records.clone();
        // Partition 1 holds the last two records; partition 0 only the first.
        for (offset, record) in records.iter_mut().skip(1).enumerate() {
            record.partition = 1;
            record.offset = i64::try_from(offset).expect("offset fits i64");
        }
        let stall = Duration::from_millis(200);
        let stream = ScriptedStream::new(records, Script::StopsAfter(2), Some(stall));

        let outcome = compio::time::timeout(
            stall * 25,
            Box::pin(recompute_usage_aggregates(&registry, &stream, base.period, &scan_cfg(1))),
        )
        .await
        .expect("the scan did not return within its test budget: a quiet stream held it open");
        match outcome {
            Err(SpendRecomputeError::Stalled { stall_ms, unfinished }) => {
                assert_eq!(stall_ms, 200);
                assert_eq!(unfinished.len(), 1, "partition 1 is short of its captured end");
                assert_eq!(
                    unfinished[0],
                    UnfinishedPartition {
                        partition: 1,
                        captured_end: 1,
                        last_read: Some(0),
                    },
                    "names the stuck partition's captured end and how far it actually got: \
                     {unfinished:?}"
                );
            }
            other => panic!("expected the scan to stall partway, got {other:?}"),
        }
        assert_total(&client, &base.app, base.period, base.total).await;
    }

    /// An in-process transport has no stall timeout because it answers every
    /// poll at once, so one poll that advances nothing while partitions are
    /// short of their ends is the stall: the scan ends there and writes
    /// nothing.
    ///
    /// WHAT IT CATCHES: treating a transport without a stall timeout as one
    /// that may be waited on forever. The scan then polls a stream that will
    /// never deliver again without end (the test's own timeout fails it).
    #[compio::test]
    async fn an_in_process_scan_that_stops_short_is_a_stall_at_once() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let base = snapshotted(&client, &[100, 50]).await;
        let stream = ScriptedStream::new(base.records.clone(), Script::StopsAfter(1), None);

        let outcome = compio::time::timeout(
            Duration::from_secs(5),
            Box::pin(recompute_usage_aggregates(&registry, &stream, base.period, &scan_cfg(8))),
        )
        .await
        .expect("the scan did not return within its test budget: it waited on an in-process log");
        match outcome {
            Err(SpendRecomputeError::Stalled { stall_ms, unfinished }) => {
                assert_eq!(stall_ms, 0, "an in-process stall names no timeout");
                assert_eq!(unfinished.len(), 1);
            }
            other => panic!("expected the short in-process scan to stall, got {other:?}"),
        }
        assert_total(&client, &base.app, base.period, base.total).await;
    }

    /// A broker that keeps delivering an offset the scan already read is not
    /// advancing: only a record past a partition's highest read offset
    /// restarts the stall clock, so the replay ends the scan at the stall
    /// timeout and nothing is written.
    ///
    /// WHAT IT CATCHES: counting any delivered record as progress. Then the
    /// replay restarts the clock on every poll and the scan never ends (the
    /// test's own timeout fails it).
    #[compio::test]
    async fn a_stream_replaying_an_offset_already_read_is_a_stall() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let base = snapshotted(&client, &[100, 50]).await;
        let stall = Duration::from_millis(200);
        let stream = ScriptedStream::new(base.records.clone(), Script::ReplaysAfter(1), Some(stall));

        let outcome = compio::time::timeout(
            stall * 25,
            Box::pin(recompute_usage_aggregates(&registry, &stream, base.period, &scan_cfg(8))),
        )
        .await
        .expect("the scan did not return within its test budget: the replay held it open");
        match outcome {
            Err(SpendRecomputeError::Stalled { unfinished, .. }) => {
                assert_eq!(unfinished.len(), 1);
            }
            other => panic!("expected the replayed scan to stall, got {other:?}"),
        }
        assert_total(&client, &base.app, base.period, base.total).await;
    }

    /// A poll that blocks its thread and answers at once without advancing -
    /// how the redpanda adapter answers once it has nothing new - never
    /// suspends, so no timeout around it can fire. The scan's own check of the
    /// stall clock between polls is what ends it, with the named error and no
    /// write.
    ///
    /// WHAT IT CATCHES: removing that check and relying on the timeout around
    /// `poll` alone. The loop then spins on the blocking poll, and the
    /// stream's watchdog fails the test by name.
    #[compio::test]
    async fn a_poll_that_answers_at_once_without_advancing_is_a_stall() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let base = snapshotted(&client, &[100, 50]).await;
        let stall = Duration::from_millis(200);
        let stream = ScriptedStream::new(base.records.clone(), Script::Blocking(1), Some(stall));

        let outcome = recompute_usage_aggregates(&registry, &stream, base.period, &scan_cfg(8))
            .await;
        match outcome {
            Err(SpendRecomputeError::Stalled { stall_ms, unfinished }) => {
                assert_eq!(stall_ms, 200);
                assert_eq!(unfinished.len(), 1);
            }
            other => panic!("expected the blocking scan to stall, got {other:?}"),
        }
        assert_total(&client, &base.app, base.period, base.total).await;
    }

    /// A partition is finished by any record at or past its captured end, and
    /// only records up to that end are read. Here the captured last offset is
    /// never delivered (the shape of a transaction marker) and the next record
    /// arrives past it: the scan completes without reading that later record.
    ///
    /// WHAT IT CATCHES: finishing a partition only on a record exactly at its
    /// captured end (the scan then stalls on the missing offset every cycle),
    /// and reading the record past the end into the totals.
    #[compio::test]
    async fn a_record_past_the_captured_end_finishes_its_partition_unread() {
        let fresh = fresh_database();
        let url = fresh.admin_url().to_string();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let base = snapshotted(&client, &[100, 50]).await;
        let organization = zeroship_core::typed_id::generate("org");
        let mut records = base.records.clone();
        records.push(record_from_event(
            3,
            event("evt_past_the_end", &base.app, &organization, 25, base.period + 20),
        ));
        let mut stream = ScriptedStream::new(records, Script::Delivers, Some(Duration::from_secs(1)));
        stream.ends = Some(vec![PartitionEnd {
            partition: 0,
            last_offset: Some(2),
        }]);

        let cycle = compio::time::timeout(
            Duration::from_secs(25),
            Box::pin(recompute_usage_aggregates(&registry, &stream, base.period, &scan_cfg(8))),
        )
        .await
        .expect("the scan did not return within its test budget")
        .expect("a record past the captured end finishes the partition");
        assert_eq!(cycle.polled, 2, "only the records up to the captured end are read");
        assert_eq!(cycle.written, 1);
        assert_total(&client, &base.app, base.period, base.total).await;
    }

    impl FakeStream {
        fn new(events: Vec<UsageEvent>) -> Self {
            let records = events
                .into_iter()
                .enumerate()
                .map(|(offset, event)| record_from_event(offset as i64, event))
                .collect();
            Self::from_records(records)
        }

        fn from_records(records: Vec<StreamRecord>) -> Self {
            Self {
                records,
                cursor: Mutex::new(0),
            }
        }
    }

    fn event(id: &str, app: &AppId, organization: &str, value: u64, event_time: i64) -> UsageEvent {
        UsageEvent {
            event_id: id.to_string(),
            source: "worker-test".to_string(),
            subject: UsageSubject {
                app: Some(app.clone()),
                organization: Some(organization.to_owned()),
            },
            meter: "requests".to_string(),
            value,
            event_time,
            dims: BTreeMap::new(),
        }
    }

    fn record_from_event(offset: i64, event: UsageEvent) -> StreamRecord {
        StreamRecord {
            partition: 0,
            offset,
            key: event.organization_subject().unwrap_or_default().as_bytes().to_vec(),
            payload: serde_json::to_vec(&event).expect("event serializes"),
        }
    }

    async fn assert_total(client: &compio_postgres::Client, app: &AppId, period: i64, total: i64) {
        let rows = client
            .query(
                "SELECT total FROM zeroship.usage_aggregates \
                 WHERE app_id = $1 AND period = $2::date AND metric = 'requests'",
                &[&app.as_str(), &period_date(period)],
            )
            .await
            .expect("read aggregate");
        assert_eq!(rows.len(), 1, "one usage_aggregates row for {}", app.as_str());
        assert_eq!(rows[0].get::<_, i64>("total"), total);
    }

    async fn assert_state(client: &compio_postgres::Client, app: &AppId, want: SpendState) {
        let rows = client
            .query(
                "SELECT state::text AS state FROM zeroship.app_spend_state WHERE app_id = $1",
                &[&app.as_str()],
            )
            .await
            .expect("read spend state");
        assert_eq!(rows.len(), 1, "one app_spend_state row for {}", app.as_str());
        let state: String = rows[0].get("state");
        assert_eq!(parse_spend_state(&state), want);
    }
}
