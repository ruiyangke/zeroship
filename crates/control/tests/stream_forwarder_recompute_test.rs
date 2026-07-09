//! Faithful stream money-path integration test for the provider-platform billing
//! rail. Uses the real memory `StreamTransport`, real event-forwarder cycle, and
//! real spend recompute snapshot writer. The provider is recording-only because
//! this test asserts the boundary call, not an external service.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::TimeZone;
use compio_postgres::{connect, NoTls};
use serde_json::json;
use uuid::Uuid;
use zeroship_control::cron::{event_forwarder, spend_recompute};
use zeroship_control::metering::provider::{
    AggregateQuery, Capabilities, IngestAck, Meter, MeteringProvider, ProviderError, UsageEvent,
    UsageSubject,
};
use zeroship_control::Registry;
use zeroship_stream::{adapters, StreamConfig, StreamRegistry};

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

#[compio::test]
async fn memory_forwarder_and_recompute_consumers_do_not_interfere() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let app = seed_app(&client).await;
    let creator = Uuid::new_v4();
    let period = chrono::Utc
        .with_ymd_and_hms(2042, 4, 1, 0, 0, 0)
        .unwrap()
        .timestamp();
    let events = vec![
        event("evt_f1_a", app, creator, "requests", 10, period + 10),
        event("evt_f1_b", app, creator, "requests", 5, period + 20),
        event("evt_f1_c", app, creator, "db_reads", 7, period + 30),
    ];

    let suffix = unique_suffix();
    let topic = format!("zeroship-control-f1-{suffix}");
    let mut stream_registry = StreamRegistry::default();
    adapters::register_builtin(&mut stream_registry);
    let streams = zeroship_control::BillingStreamConfig::new(
        Arc::new(stream_registry),
        "memory",
        StreamConfig::from(json!({
            "topic": topic.clone(),
            "group.id": "base-group-overridden",
            "partitions": 4
        })),
        format!("billing-forwarder-{suffix}"),
        format!("spend-recompute-witness-{suffix}"),
    )
    .expect("billing stream config builds");

    let forwarder_stream = streams.build_forwarder().expect("forwarder stream");
    let recompute_stream = streams.build_recompute().expect("recompute stream");
    for event in &events {
        let payload = serde_json::to_vec(event).expect("usage event serializes");
        forwarder_stream
            .publish(
                &topic,
                event.subject.app.unwrap().to_string().as_bytes(),
                &payload,
            )
            .await
            .expect("publish usage event");
    }

    let provider = Arc::new(RecordingProvider::default());
    let stack = zeroship_control::metering::provider::BillingStack::with_meter_for_tests(
        provider.clone(),
    );
    let dead_letters = NoopDeadLetters;
    let forward_cfg = event_forwarder::EventForwarderConfig {
        batch_max: 100,
        idle_sleep: Duration::from_millis(1),
        retry_backoff: Duration::from_millis(1),
        max_retry_backoff: Duration::from_millis(1),
    };

    let forwarded = event_forwarder::run_cycle(
        forwarder_stream.as_ref(),
        &stack,
        &dead_letters,
        &forward_cfg,
    )
    .await
    .expect("forwarder cycle");
    assert_eq!(forwarded.polled, events.len());
    assert_eq!(forwarded.ingested, events.len());
    assert_eq!(forwarded.committed, events.len());
    assert_eq!(provider.events(), events, "provider saw each usage event once");

    let recompute_cfg = spend_recompute::SpendRecomputeConfig {
        interval: Duration::from_secs(1),
        settle_window: Duration::from_secs(1),
        batch_max: 2,
    };
    let recomputed = spend_recompute::recompute_usage_aggregates(
        &registry,
        recompute_stream.as_ref(),
        period,
        &recompute_cfg,
    )
    .await
    .expect("spend recompute");
    assert_eq!(recomputed.polled, events.len());
    assert_eq!(recomputed.decoded, events.len());
    assert_eq!(recomputed.skipped, 0);
    assert_eq!(recomputed.aggregates, 2);
    assert_eq!(recomputed.written, 2);
    assert_total(&client, app, period, "requests", 15).await;
    assert_total(&client, app, period, "db_reads", 7).await;

    let fresh_forwarder_stream = streams.build_forwarder().expect("fresh forwarder stream");
    let replay = event_forwarder::run_cycle(
        fresh_forwarder_stream.as_ref(),
        &stack,
        &dead_letters,
        &forward_cfg,
    )
    .await
    .expect("forwarder replay check");
    assert_eq!(
        replay.polled, 0,
        "fresh forwarder consumer must resume at its committed offset after recompute rewind"
    );
    assert_eq!(
        provider.events(),
        events,
        "recompute rewind must not cause already-committed events to be forwarded again"
    );
}

#[derive(Debug, Default)]
struct RecordingProvider {
    events: Mutex<Vec<UsageEvent>>,
}

impl RecordingProvider {
    fn events(&self) -> Vec<UsageEvent> {
        self.events
            .lock()
            .expect("recording provider lock poisoned")
            .clone()
    }
}

#[async_trait::async_trait(?Send)]
impl Meter for RecordingProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        self.events
            .lock()
            .expect("recording provider lock poisoned")
            .extend_from_slice(batch);
        Ok(IngestAck {
            accepted: batch.len(),
            deduped: Some(0),
        })
    }

    async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
        Ok(0)
    }
}

impl MeteringProvider for RecordingProvider {
    fn id(&self) -> &str {
        "recording"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::METER
    }

    fn as_meter(&self) -> Option<&dyn Meter> {
        Some(self)
    }
}

#[derive(Debug)]
struct NoopDeadLetters;

#[async_trait::async_trait(?Send)]
impl event_forwarder::DeadLetterSink for NoopDeadLetters {
    async fn record_provider_reject(
        &self,
        _entry: event_forwarder::ProviderDeadLetter,
    ) -> Result<(), event_forwarder::EventForwarderError> {
        Ok(())
    }

    async fn record_decode_error(
        &self,
        _entry: event_forwarder::DecodeDeadLetter,
    ) -> Result<(), event_forwarder::EventForwarderError> {
        Ok(())
    }
}

async fn seed_app(client: &compio_postgres::Client) -> Uuid {
    let plan_id = format!("pln_f1_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'f1', 0, 0, 1000000000000, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 0)",
            &[&plan_id],
        )
        .await
        .expect("seed plan");
    client
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[
                &format!("f1-{}", Uuid::new_v4().simple()),
                &plan_id,
                &Uuid::new_v4().to_string(),
            ],
        )
        .await
        .expect("insert app")[0]
        .get("id")
}

fn event(
    id: &str,
    app: Uuid,
    creator: Uuid,
    meter: &str,
    value: u64,
    event_time: i64,
) -> UsageEvent {
    UsageEvent {
        event_id: format!("{}-{}", id, Uuid::new_v4().simple()),
        source: "worker-test".to_string(),
        subject: UsageSubject {
            app: Some(app),
            creator,
        },
        meter: meter.to_string(),
        value,
        event_time,
        dims: Default::default(),
    }
}

async fn assert_total(
    client: &compio_postgres::Client,
    app: Uuid,
    period: i64,
    metric: &str,
    total: i64,
) {
    let rows = client
        .query(
            "SELECT total FROM zeroship.usage_aggregates \
             WHERE app_id = $1 AND period = $2::date AND metric = $3",
            &[&app, &period_date(period), &metric],
        )
        .await
        .expect("read aggregate");
    assert_eq!(rows.len(), 1, "one usage_aggregates row for {app}/{metric}");
    assert_eq!(rows[0].get::<_, i64>("total"), total);
}

fn unique_suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis();
    format!("{}-{now}", std::process::id())
}

fn period_date(period_start_unix: i64) -> chrono::NaiveDate {
    use chrono::Datelike;
    let dt = chrono::Utc
        .timestamp_opt(period_start_unix, 0)
        .single()
        .expect("valid period timestamp");
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1)
        .expect("valid billing period date")
}
