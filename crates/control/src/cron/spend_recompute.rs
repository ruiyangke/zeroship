//! Periodic spend recompute: retained stream → `usage_aggregates` snapshot →
//! existing spend evaluation.
//!
//! v7 enforcement intentionally runs at a tunable cadence (default hourly). The
//! recompute reads the current billing period from the retained usage-event
//! stream, computes a full `SUM(value)` snapshot per `(app_id, metric)`, writes
//! that snapshot into `usage_aggregates`, and then triggers the existing spend
//! evaluator so the gateway can keep enforcing `app_spend_state`.

use std::collections::HashMap;
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
/// the next cadence; no partial snapshot is written unless the full scan
/// completed successfully.
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
/// `SpendEngine::evaluate_all` under the multi-instance advisory lock).
#[allow(clippy::future_not_send)]
pub async fn tick(
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

    loop {
        let records = stream.poll(batch_max).await?;
        if records.is_empty() {
            break;
        }
        cycle.polled += records.len();
        for record in &records {
            match decode_record(record) {
                Ok(event) => {
                    cycle.decoded += 1;
                    if apply_event(&mut totals, &event, period_start) {
                        continue;
                    }
                    cycle.skipped += 1;
                }
                Err(err) => return Err(err),
            }
        }
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

#[cfg(test)]
mod tests {
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

    fn db_url() -> Option<String> {
        std::env::var("CONTROL_TEST_DB").ok()
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
        let Some(url) = db_url() else {
            eprintln!("skip: CONTROL_TEST_DB not set");
            return;
        };
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

    #[compio::test]
    async fn unsettled_period_recompute_rewrites_current_and_previous_snapshots() {
        let Some(url) = db_url() else {
            eprintln!("skip: CONTROL_TEST_DB not set");
            return;
        };
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
                .map(|(offset, event)| StreamRecord {
                    partition: 0,
                    offset: offset as i64,
                    key: event.creator_subject().into_bytes(),
                    payload: serde_json::to_vec(&event).expect("event serializes"),
                })
                .collect();
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
