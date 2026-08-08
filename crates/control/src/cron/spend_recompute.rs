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

use uuid::Uuid;
use zeroship_core::usage_event::UsageEvent;
use zeroship_stream::{StreamRecord, StreamTransport};

use crate::metering::{period_start_unix, Metering, UsageAggregate};
use crate::registry::{Registry, RegistryError};
use crate::AppState;

pub const DEFAULT_RECOMPUTE_INTERVAL_SECS: u64 = 60 * 60;
pub const DEFAULT_BATCH_MAX: usize = 10_000;

/// Consecutive empty polls that conclude the retained stream is fully drained
/// for one recompute cycle. A Kafka-wire broker's fetch is async, so a single
/// empty poll right after `rewind` does not mean the topic is empty.
const RECOMPUTE_DRAIN_EMPTY_ROUNDS: u32 = 3;

/// Stable `pg_advisory_lock` key that single-flights the recompute fleet-wide.
///
/// Same encoding as every other sweep key in this module tree: `0x7a73` is
/// ASCII "zs", then four bytes naming the sweep ("rcmp"), then a version
/// nibble. The convention matters more than usual here - two sweeps sharing a
/// key would block each other fleet-wide, and the symptom would be a cron that
/// mysteriously never runs rather than an error. `lock_keys_do_not_collide`
/// pins it.
const RECOMPUTE_ADVISORY_LOCK_KEY: i64 = 0x7a73_7263_6d70_0001;

#[derive(Debug, Clone)]
pub struct SpendRecomputeConfig {
    pub interval: Duration,
    pub settle_window: Duration,
    pub batch_max: usize,
}

impl Default for SpendRecomputeConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_RECOMPUTE_INTERVAL_SECS),
            settle_window: Duration::from_secs(
                super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: DEFAULT_BATCH_MAX,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SpendRecomputeCycle {
    pub polled: usize,
    pub decoded: usize,
    pub skipped: usize,
    pub skipped_undecodable: usize,
    pub aggregates: usize,
    pub written: usize,
    pub transitions: usize,
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
}

impl From<RegistryError> for SpendRecomputeError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value.to_string())
    }
}

/// Cron entry point. A transient stream/PG error is logged and retried after
/// the next cadence.
///
/// This used to claim that "no partial snapshot is written unless the full scan
/// completed successfully". Nothing in this function knows whether a scan
/// completed. The drain loop's ONLY exit is `RECOMPUTE_DRAIN_EMPTY_ROUNDS`
/// consecutive empty polls, and `replace_period_snapshot` DELETEs the whole
/// period and rewrites it from whatever that scan happened to see - so a
/// partial snapshot is written on every cycle that reads anything at all. The
/// `polled == 0` guard below covers the read-nothing case and nothing else.
///
/// What the empty-poll heuristic actually measures is LATENCY, not
/// end-of-stream: the redpanda adapter returns immediately once it has any
/// record and only blocks `poll_timeout_ms` when it has none, so three empty
/// rounds is roughly `3 x poll_timeout_ms` of tolerance. A mid-scan fetch stall
/// longer than that - a partition leader failover, a broker pause - reads as
/// "the topic ended here", and the period is rewritten from the truncated read.
///
/// No test can currently distinguish the two: every stream double in this
/// repository is empty-forever once drained, so one empty poll IS end-of-stream
/// for all of them and the constant could be 1 without any test noticing.
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
                    skipped = cycle.skipped,
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
/// The recompute half now takes a lock too. It was the only periodic sweep in
/// this crate without one - billing_reconcile, stripe_reconcile, billing_notify
/// and workflow_blob_gc all single-flight - and since each replica got its own
/// consumer group, every replica reads the COMPLETE retained stream and writes
/// the same snapshot. Correct, but N replicas each doing O(period) work per
/// tick with N concurrent `replace_period_snapshot` transactions contending on
/// the same rows.
///
/// ORDERING NOTE, because this lock would have been actively WRONG before the
/// per-replica group split: under the old shared group the loser held partition
/// assignments it would then never read, so single-flighting would have made
/// the winner's snapshot partial. It is safe now precisely because a loser's
/// group has no other members, so skipping its poll leaves nothing unread.
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
            &[&RECOMPUTE_ADVISORY_LOCK_KEY],
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
        let mut cycle = SpendRecomputeCycle::default();
        cycle.transitions = super::spend_reconcile::tick(state).await?;
        return Ok(cycle);
    }

    let result = recompute_and_reconcile(state, stream, cfg).await;

    if let Err(e) = lock_conn
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&RECOMPUTE_ADVISORY_LOCK_KEY],
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
#[allow(clippy::future_not_send)]
pub async fn recompute_and_reconcile(
    state: &AppState,
    stream: &dyn StreamTransport,
    cfg: &SpendRecomputeConfig,
) -> Result<SpendRecomputeCycle, SpendRecomputeError> {
    let mut cycle = recompute_unsettled_period_snapshots(
        &state.registry,
        stream,
        chrono::Utc::now().timestamp(),
        cfg,
    )
    .await?;
    cycle.transitions = super::spend_reconcile::tick(state).await?;
    Ok(cycle)
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
        total.skipped += next.skipped;
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

    let batch_max = cfg.batch_max.max(1);
    let mut cycle = SpendRecomputeCycle::default();
    let mut totals = HashMap::<(Uuid, String), i64>::new();
    let mut seen_event_ids = HashSet::<String>::new();

    // A Kafka-wire broker's fetch is asynchronous: right after `rewind`'s
    // seek-to-beginning, the first `poll` can return empty even though the topic
    // is non-empty (the fetch has not landed yet). A single empty poll therefore
    // does NOT mean "fully drained" — tolerate a few consecutive empties so each
    // cycle reads the COMPLETE retained stream, rather than replacing the period
    // snapshot with a partial (or empty) read.
    let mut empty_rounds = 0u32;
    loop {
        let records = stream.poll(batch_max).await?;
        if records.is_empty() {
            empty_rounds += 1;
            if empty_rounds >= RECOMPUTE_DRAIN_EMPTY_ROUNDS {
                break;
            }
            continue;
        }
        empty_rounds = 0;
        cycle.polled += records.len();
        for record in &records {
            match decode_record(record) {
                Ok(event) => {
                    cycle.decoded += 1;
                    if !seen_event_ids.insert(event.event_id.clone()) {
                        cycle.skipped += 1;
                        continue;
                    }
                    if apply_event(&mut totals, &event, period_start) {
                        continue;
                    }
                    cycle.skipped += 1;
                }
                Err(SpendRecomputeError::Decode {
                    partition,
                    offset,
                    source,
                }) => {
                    cycle.skipped_undecodable += 1;
                    tracing::warn!(
                        partition,
                        offset,
                        error = %source,
                        "spend_recompute: skipping undecodable usage stream record"
                    );
                }
                Err(err) => return Err(err),
            }
        }
    }

    // A cycle that read NOTHING must not overwrite the snapshot: replacing it
    // with an empty set would transiently zero out enforcement (an app would
    // briefly look like it has no usage and escape its spend limit). Leave the
    // last good snapshot in place until a cycle actually reads the stream.
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
            .as_bytes()
            .cmp(b.app_id.as_bytes())
            .then_with(|| a.metric.cmp(&b.metric))
    });
    cycle.aggregates = aggregates.len();
    cycle.written = Metering::new(registry.clone())
        .replace_period_snapshot(period_start, &aggregates)
        .await?;
    Ok(cycle)
}

fn apply_event(
    totals: &mut HashMap<(Uuid, String), i64>,
    event: &UsageEvent,
    period_start: i64,
) -> bool {
    if period_start_unix(event.event_time) != period_start {
        return false;
    }
    let Some(app_id) = event.subject.app else {
        tracing::warn!(
            event_id = %event.event_id,
            meter = %event.meter,
            "spend_recompute: usage event has no app subject — skipping"
        );
        return false;
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
        return false;
    }

    let total = totals.entry((app_id, event.meter.clone())).or_insert(0);
    match total.checked_add(value) {
        Some(sum) => *total = sum,
        None => {
            *total = i64::MAX;
            tracing::warn!(
                app_id = %app_id,
                meter = %event.meter,
                "spend_recompute: aggregate total overflow — saturating to i64::MAX for conservative enforcement"
            );
        }
    }
    true
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

    /// Two sweeps sharing an advisory-lock key would block each other
    /// fleet-wide, and the symptom is a cron that silently never runs rather
    /// than anything that errors - so the collision is worth pinning.
    ///
    /// WHAT THIS DOES NOT CATCH, and it is most of the space: the six other
    /// keys are `const` PRIVATE to their own modules, so they cannot be
    /// imported and are reproduced here as literals. That means this test sees
    /// a NEW key added in this file, and nothing else. If someone changes
    /// `billing_notify`'s key to collide with this one, this test still
    /// passes. Closing that needs the keys centralised in one module, which is
    /// filed separately rather than done here.
    ///
    /// Values transcribed 2026-08-07 from: billing_reconcile.rs:95 and :101,
    /// billing_notify.rs:56, stripe_reconcile.rs:75, spend_reconcile.rs:33,
    /// dunning.rs:43.
    #[test]
    fn lock_keys_do_not_collide() {
        let others: [(i64, &str); 6] = [
            (0x7a73_6269_6c6c_0001, "billing sweep"),
            (0x7a73_6273_6166_0001, "billing safety net"),
            (0x7a73_6e6f_7466_0001, "billing notify"),
            (0x7a73_7265_636f_0001, "stripe reconcile"),
            (0x7a73_7370_6e64_0001, "spend sweep"),
            (0x7a73_6475_6e6e_0001, "dunning sweep"),
        ];
        for (key, name) in others {
            assert_ne!(
                RECOMPUTE_ADVISORY_LOCK_KEY, key,
                "recompute lock key collides with the {name} key; both sweeps would \
                 block each other fleet-wide and neither would report an error"
            );
        }
    }

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
        let app = uuid::Uuid::new_v4();
        let creator = uuid::Uuid::new_v4();
        let period = period_start_unix(1_783_468_800);
        let previous = period_start_unix(period - 1);
        assert_ne!(period, previous, "the two stamps must be in different periods");

        let mk = |id: &str, value: u64, event_time: i64| zeroship_core::usage_event::UsageEvent {
            event_id: id.to_string(),
            source: "worker-test".to_string(),
            subject: zeroship_core::usage_event::UsageSubject {
                app: Some(app),
                creator,
            },
            meter: "requests".to_string(),
            value,
            event_time,
            dims: std::collections::BTreeMap::new(),
        };

        let mut totals = HashMap::new();

        assert!(
            apply_event(&mut totals, &mk("evt_in", 100, period + 10), period),
            "an in-period event must be counted"
        );
        assert_eq!(totals.get(&(app, "requests".to_string())), Some(&100));

        assert!(
            !apply_event(&mut totals, &mk("evt_late", 5_000, previous + 10), period),
            "an event from a settled period must not be counted"
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
/// `required-features` in Cargo.toml gates whole targets and cannot reach inside
/// a lib, so the gate is spelled as a `cfg` here. It is the same
/// `live-db-tests` feature the crate's 44 gated integration targets carry, and
/// the same single `cargo test -p zeroship-control --features live-db-tests`
/// invocation runs it - the lib target is built with the feature too, so these
/// cases need no separate entry anywhere.
#[cfg(all(test, feature = "live-db-tests"))]
mod live_db_tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use chrono::TimeZone;
    use compio_postgres::{connect, NoTls};
    use zeroship_core::types::SpendState;
    use zeroship_core::usage_event::UsageSubject;
    use zeroship_stream::{StreamError, StreamOffset};

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
    }

    fn db_url() -> String {
        std::env::var("CONTROL_TEST_DB")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| {
                "postgresql://postgres:zeroship@localhost:5440/zeroship_billing_test".to_string()
            })
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
    ) -> Uuid {
        let name = format!("{label}-{}", Uuid::new_v4());
        client
            .query(
                "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
                 VALUES ($1, $2, $3, '') RETURNING id",
                &[&name, &plan_id, &Uuid::new_v4().to_string()],
            )
            .await
            .expect("insert app")[0]
            .get("id")
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
        let plan_id = format!("pln_recompute_{}", Uuid::new_v4().simple());
        let fx_one_cent: i64 = 1_000_000_000_000;
        client
            .execute(
                "INSERT INTO zeroship.plans \
                   (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                    runtime_limits_json, spend_limit_default_cents) \
                 VALUES ($1, 'recompute-test', 0, 0, $2, \
                         '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100)",
                &[&plan_id, &fx_one_cent],
            )
            .await
            .expect("seed recompute plan");
        plan_id
    }

    #[compio::test]
    async fn stream_recompute_replaces_usage_aggregates_and_spend_state_idempotently() {
        let url = db_url();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let period = current_period_start_unix();
        let plan_id = seed_pricing(&client).await;
        let warn_app = seed_priced_app(&client, &plan_id, "recompute-warn").await;
        let degrade_app = seed_priced_app(&client, &plan_id, "recompute-degrade").await;
        let block_app = seed_priced_app(&client, &plan_id, "recompute-block").await;
        let creator = Uuid::new_v4();
        let stream = FakeStream::new(vec![
            event("evt_warn_a", warn_app, creator, 30, period + 10),
            event("evt_warn_b", warn_app, creator, 50, period + 11),
            event("evt_degrade", degrade_app, creator, 95, period + 12),
            event("evt_block", block_app, creator, 100, period + 13),
            event("evt_old_period", block_app, creator, 999, period.saturating_sub(60)),
        ]);
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(
                super::super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: 2,
        };

        let first = recompute_usage_aggregates(&registry, &stream, period, &cfg)
            .await
            .expect("first recompute");
        assert_eq!(first.polled, 5);
        assert_eq!(first.decoded, 5);
        assert_eq!(first.skipped, 1);
        assert_eq!(first.aggregates, 3);
        assert_eq!(first.written, 3);
        assert_total(&client, warn_app, period, 80).await;
        assert_total(&client, degrade_app, period, 95).await;
        assert_total(&client, block_app, period, 100).await;

        let second = recompute_usage_aggregates(&registry, &stream, period, &cfg)
            .await
            .expect("second recompute");
        assert_eq!(second.written, 3);
        assert_total(&client, warn_app, period, 80).await;
        assert_total(&client, degrade_app, period, 95).await;
        assert_total(&client, block_app, period, 100).await;

        let transitions = SpendEngine::new(registry)
            .evaluate_all()
            .await
            .expect("evaluate_all");
        assert!(transitions.iter().any(|t| t.app_id == warn_app));
        assert!(transitions.iter().any(|t| t.app_id == degrade_app));
        assert!(transitions.iter().any(|t| t.app_id == block_app));
        assert_state(&client, warn_app, SpendState::Warn).await;
        assert_state(&client, degrade_app, SpendState::Degrade).await;
        assert_state(&client, block_app, SpendState::Block).await;
    }

    #[compio::test]
    async fn stream_recompute_empty_cycle_does_not_wipe_snapshot() {
        // A recompute cycle that reads NOTHING (a transient empty poll — common
        // right after a Kafka-wire rewind) must leave the last good snapshot in
        // place, not overwrite it with zero. Otherwise enforcement flickers to
        // "no usage" and an over-limit app briefly escapes its spend limit.
        let url = db_url();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let period = current_period_start_unix();
        let plan_id = seed_pricing(&client).await;
        let app = seed_priced_app(&client, &plan_id, "recompute-nowipe").await;
        let creator = Uuid::new_v4();
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(
                super::super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: 8,
        };

        // Cycle 1: real usage lands.
        let populated = FakeStream::new(vec![event("evt_real", app, creator, 100, period + 10)]);
        let first = recompute_usage_aggregates(&registry, &populated, period, &cfg)
            .await
            .expect("populated recompute");
        assert_eq!(first.written, 1);
        assert_total(&client, app, period, 100).await;

        // Cycle 2: an EMPTY stream must NOT wipe the snapshot.
        let empty = FakeStream::new(vec![]);
        let second = recompute_usage_aggregates(&registry, &empty, period, &cfg)
            .await
            .expect("empty recompute");
        assert_eq!(second.polled, 0);
        assert_eq!(second.written, 0, "an empty cycle writes nothing");
        assert_total(&client, app, period, 100).await; // snapshot preserved
    }

    #[compio::test]
    async fn stream_recompute_dedups_duplicate_event_ids() {
        let url = db_url();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let period = current_period_start_unix();
        let plan_id = seed_pricing(&client).await;
        let app = seed_priced_app(&client, &plan_id, "recompute-dedup").await;
        let creator = Uuid::new_v4();
        let stream = FakeStream::new(vec![
            event("evt_duplicate_replay", app, creator, 40, period + 10),
            event("evt_duplicate_replay", app, creator, 40, period + 10),
            event("evt_distinct", app, creator, 2, period + 11),
        ]);
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(
                super::super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: 2,
        };

        let cycle = recompute_usage_aggregates(&registry, &stream, period, &cfg)
            .await
            .expect("recompute with duplicate event_id");
        assert_eq!(cycle.polled, 3);
        assert_eq!(cycle.decoded, 3);
        assert_eq!(cycle.skipped, 1);
        assert_eq!(cycle.aggregates, 1);
        assert_eq!(cycle.written, 1);
        assert_total(&client, app, period, 42).await;
    }

    #[compio::test]
    async fn stream_recompute_skips_undecodable_records() {
        let url = db_url();
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let period = current_period_start_unix();
        let plan_id = seed_pricing(&client).await;
        let app = seed_priced_app(&client, &plan_id, "recompute-poison").await;
        let creator = Uuid::new_v4();
        let stream = FakeStream::from_records(vec![
            record_from_event(0, event("evt_before_poison", app, creator, 10, period + 10)),
            StreamRecord {
                partition: 0,
                offset: 1,
                key: b"poison".to_vec(),
                payload: b"{not-json".to_vec(),
            },
            record_from_event(2, event("evt_after_poison", app, creator, 7, period + 11)),
        ]);
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(
                super::super::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS,
            ),
            batch_max: 2,
        };

        let cycle = recompute_usage_aggregates(&registry, &stream, period, &cfg)
            .await
            .expect("recompute skips undecodable stream records");
        assert_eq!(cycle.polled, 3);
        assert_eq!(cycle.decoded, 2);
        assert_eq!(cycle.skipped, 0);
        assert_eq!(cycle.skipped_undecodable, 1);
        assert_eq!(cycle.aggregates, 1);
        assert_eq!(cycle.written, 1);
        assert_total(&client, app, period, 17).await;
    }

    #[compio::test]
    async fn unsettled_period_recompute_rewrites_current_and_previous_snapshots() {
        let url = db_url();
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
        let creator = Uuid::new_v4();
        let stream = FakeStream::new(vec![
            event("evt_prev_unsettled", app, creator, 41, previous + 10),
            event("evt_current_unsettled", app, creator, 59, current + 10),
        ]);
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(3600),
            batch_max: 1,
        };

        let cycle = recompute_unsettled_period_snapshots(&registry, &stream, now, &cfg)
            .await
            .expect("recompute unsettled periods");
        assert_eq!(
            cycle.polled, 4,
            "each period recompute scans the retained stream"
        );
        assert_eq!(cycle.decoded, 4);
        assert_eq!(cycle.skipped, 2);
        assert_eq!(cycle.aggregates, 2);
        assert_eq!(cycle.written, 2);
        assert_total(&client, app, previous, 41).await;
        assert_total(&client, app, current, 59).await;
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

    fn event(id: &str, app: Uuid, creator: Uuid, value: u64, event_time: i64) -> UsageEvent {
        UsageEvent {
            event_id: id.to_string(),
            source: "worker-test".to_string(),
            subject: UsageSubject {
                app: Some(app),
                creator,
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
            key: event.creator_subject().into_bytes(),
            payload: serde_json::to_vec(&event).expect("event serializes"),
        }
    }

    async fn assert_total(client: &compio_postgres::Client, app: Uuid, period: i64, total: i64) {
        let rows = client
            .query(
                "SELECT total FROM zeroship.usage_aggregates \
                 WHERE app_id = $1 AND period = $2::date AND metric = 'requests'",
                &[&app, &period_date(period)],
            )
            .await
            .expect("read aggregate");
        assert_eq!(rows.len(), 1, "one usage_aggregates row for {app}");
        assert_eq!(rows[0].get::<_, i64>("total"), total);
    }

    async fn assert_state(client: &compio_postgres::Client, app: Uuid, want: SpendState) {
        let rows = client
            .query(
                "SELECT state::text AS state FROM zeroship.app_spend_state WHERE app_id = $1",
                &[&app],
            )
            .await
            .expect("read spend state");
        assert_eq!(rows.len(), 1, "one app_spend_state row for {app}");
        let state: String = rows[0].get("state");
        assert_eq!(parse_spend_state(&state), want);
    }
}
