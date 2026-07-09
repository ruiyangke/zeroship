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

fn db_url(test_name: &str) -> Option<String> {
    match std::env::var("CONTROL_TEST_DB") {
        Ok(url) => Some(url),
        Err(_) => {
            loud_control_db_skip(test_name);
            None
        }
    }
}

fn loud_control_db_skip(test_name: &str) {
    use std::io::Write as _;

    let msg = format!(
        "\n================ BILLING DB TEST SKIPPED ================\n\
         {test_name}: CONTROL_TEST_DB is unset; this test did not exercise Postgres.\n\
         Export CONTROL_TEST_DB=postgres://postgres:zeroship@localhost:5440/control_billing_test\n\
         or run tests/run_billing_suite.sh for the billing gate.\n\
         ==========================================================\n"
    );
    let _ = std::io::stderr().write_all(msg.as_bytes());
}

async fn pg(db_url: &str) -> Arc<compio_postgres::Client> {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    Arc::new(client)
}

#[compio::test]
async fn memory_forwarder_and_recompute_consumers_do_not_interfere() {
    let Some(url) = db_url("memory_forwarder_and_recompute_consumers_do_not_interfere") else {
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

#[compio::test]
async fn memory_forwarder_redelivers_uncommitted_tail_after_mid_batch_failure() {
    let suffix = unique_suffix();
    let topic = format!("zeroship-control-f4-crash-{suffix}");
    let group = format!("billing-forwarder-crash-{suffix}");
    let mut stream_registry = StreamRegistry::default();
    adapters::register_builtin(&mut stream_registry);
    let config = StreamConfig::from(json!({
        "topic": topic.clone(),
        "group.id": group,
        "partitions": 1
    }));

    let initial_stream = stream_registry
        .build("memory", &config)
        .expect("initial memory stream");
    let app = Uuid::new_v4();
    let creator = Uuid::new_v4();
    let period = chrono::Utc
        .with_ymd_and_hms(2042, 5, 1, 0, 0, 0)
        .unwrap()
        .timestamp();
    let events: Vec<_> = (0..5)
        .map(|i| {
            event(
                &format!("evt_f4_crash_{i}"),
                app,
                creator,
                "requests",
                1,
                period + i64::from(i),
            )
        })
        .collect();
    for event in &events {
        initial_stream
            .publish(
                &topic,
                event.subject.app.unwrap().to_string().as_bytes(),
                &serde_json::to_vec(event).expect("usage event serializes"),
            )
            .await
            .expect("publish usage event");
    }

    let provider = Arc::new(CrashAfterProvider::new(2));
    let provider_for_stack: Arc<dyn MeteringProvider> = provider.clone();
    let stack = zeroship_control::metering::provider::BillingStack::with_meter_for_tests(
        provider_for_stack,
    );
    let dead_letters = NoopDeadLetters;
    let forward_cfg = event_forwarder::EventForwarderConfig {
        batch_max: 100,
        idle_sleep: Duration::from_millis(1),
        retry_backoff: Duration::from_millis(1),
        max_retry_backoff: Duration::from_millis(1),
    };

    let first = event_forwarder::run_cycle(
        initial_stream.as_ref(),
        &stack,
        &dead_letters,
        &forward_cfg,
    )
    .await
    .expect_err("first cycle crashes after partially applying the batch");
    assert!(
        first.to_string().contains("injected mid-batch crash"),
        "{first}"
    );
    assert_eq!(
        provider.applied_ids(),
        events[..2]
            .iter()
            .map(|event| event.event_id.clone())
            .collect::<Vec<_>>(),
        "the simulated crash applied only the prefix before any offset commit"
    );

    provider.allow_success();
    let restarted_stream = stream_registry
        .build("memory", &config)
        .expect("fresh memory stream for same group");
    let second = event_forwarder::run_cycle(
        restarted_stream.as_ref(),
        &stack,
        &dead_letters,
        &forward_cfg,
    )
    .await
    .expect("restart replays uncommitted batch");

    assert_eq!(second.polled, events.len());
    assert_eq!(second.ingested, events.len() - 2);
    assert_eq!(second.deduped, Some(2));
    assert_eq!(second.committed, events.len());
    assert_eq!(
        provider.applied_ids(),
        events
            .iter()
            .map(|event| event.event_id.clone())
            .collect::<Vec<_>>(),
        "provider dedup avoids double-apply and the unacked tail has no gap"
    );

    let after_commit = stream_registry
        .build("memory", &config)
        .expect("post-commit memory stream for same group");
    let third = event_forwarder::run_cycle(
        after_commit.as_ref(),
        &stack,
        &dead_letters,
        &forward_cfg,
    )
    .await
    .expect("post-commit cycle");
    assert_eq!(third.polled, 0, "committed offsets are not re-forwarded");
}

#[compio::test]
async fn pg_dead_letter_sink_persists_provider_reject_and_decode_failure() {
    let Some(url) = db_url("pg_dead_letter_sink_persists_provider_reject_and_decode_failure") else {
        return;
    };
    let client = pg(&url).await;
    let suffix = unique_suffix();
    let mut stream_registry = StreamRegistry::default();
    adapters::register_builtin(&mut stream_registry);
    let forward_cfg = event_forwarder::EventForwarderConfig {
        batch_max: 100,
        idle_sleep: Duration::from_millis(1),
        retry_backoff: Duration::from_millis(1),
        max_retry_backoff: Duration::from_millis(1),
    };

    let reject_topic = format!("zeroship-control-f4-reject-{suffix}");
    let reject_config = StreamConfig::from(json!({
        "topic": reject_topic.clone(),
        "group.id": format!("billing-forwarder-reject-{suffix}"),
        "partitions": 1
    }));
    let reject_stream = stream_registry
        .build("memory", &reject_config)
        .expect("reject memory stream");
    let reject_provider = Arc::new(RejectingProvider {
        id: format!("rejecting-{suffix}"),
    });
    let reject_provider_for_stack: Arc<dyn MeteringProvider> = reject_provider.clone();
    let reject_stack = zeroship_control::metering::provider::BillingStack::with_meter_for_tests(
        reject_provider_for_stack,
    );
    let app = Uuid::new_v4();
    let creator = Uuid::new_v4();
    let event = event(
        "evt_f4_pg_reject",
        app,
        creator,
        "requests",
        1,
        1_783_468_800,
    );
    reject_stream
        .publish(
            &reject_topic,
            app.to_string().as_bytes(),
            &serde_json::to_vec(&event).expect("usage event serializes"),
        )
        .await
        .expect("publish reject event");
    let sink = event_forwarder::PgDeadLetterSink::new(Arc::clone(&client));
    let reject_cycle = event_forwarder::run_cycle(
        reject_stream.as_ref(),
        &reject_stack,
        &sink,
        &forward_cfg,
    )
    .await
    .expect("provider reject is quarantined");
    assert_eq!(reject_cycle.dead_lettered, 1);
    assert_eq!(reject_cycle.committed, 1);
    assert_dead_letter_row(
        &client,
        reject_provider.id(),
        &event.event_id,
        "provider permanent reject 400: unknown subject",
    )
    .await;

    let decode_topic = format!("zeroship-control-f4-decode-{suffix}");
    let decode_config = StreamConfig::from(json!({
        "topic": decode_topic.clone(),
        "group.id": format!("billing-forwarder-decode-{suffix}"),
        "partitions": 1
    }));
    let decode_stream = stream_registry
        .build("memory", &decode_config)
        .expect("decode memory stream");
    let decode_provider = Arc::new(RecordingProvider::with_id(format!("decode-{suffix}")));
    let decode_provider_for_stack: Arc<dyn MeteringProvider> = decode_provider.clone();
    let decode_stack = zeroship_control::metering::provider::BillingStack::with_meter_for_tests(
        decode_provider_for_stack,
    );
    decode_stream
        .publish(&decode_topic, b"decode-key", b"{not-json")
        .await
        .expect("publish decode poison");
    let decode_cycle = event_forwarder::run_cycle(
        decode_stream.as_ref(),
        &decode_stack,
        &sink,
        &forward_cfg,
    )
    .await
    .expect("decode failure is quarantined");
    assert_eq!(decode_cycle.dead_lettered, 1);
    assert_eq!(decode_cycle.committed, 1);
    assert_dead_letter_row(&client, decode_provider.id(), "decode:0:0", "decode_error").await;
}

#[derive(Debug)]
struct RecordingProvider {
    id: String,
    events: Mutex<Vec<UsageEvent>>,
}

impl Default for RecordingProvider {
    fn default() -> Self {
        Self::with_id("recording".to_string())
    }
}

impl RecordingProvider {
    fn with_id(id: String) -> Self {
        Self {
            id,
            events: Mutex::new(Vec::new()),
        }
    }

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
        &self.id
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::METER
    }

    fn as_meter(&self) -> Option<&dyn Meter> {
        Some(self)
    }
}

#[derive(Debug)]
struct RejectingProvider {
    id: String,
}

#[async_trait::async_trait(?Send)]
impl Meter for RejectingProvider {
    async fn ingest(&self, _batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        Err(ProviderError::permanent_reject(400, "unknown subject"))
    }

    async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
        Ok(0)
    }
}

impl MeteringProvider for RejectingProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::METER
    }

    fn as_meter(&self) -> Option<&dyn Meter> {
        Some(self)
    }
}

#[derive(Debug)]
struct CrashAfterProvider {
    fail_after: Mutex<Option<usize>>,
    applied_ids: Mutex<Vec<String>>,
}

impl CrashAfterProvider {
    fn new(fail_after: usize) -> Self {
        Self {
            fail_after: Mutex::new(Some(fail_after)),
            applied_ids: Mutex::new(Vec::new()),
        }
    }

    fn allow_success(&self) {
        *self
            .fail_after
            .lock()
            .expect("crash provider fail_after poisoned") = None;
    }

    fn applied_ids(&self) -> Vec<String> {
        self.applied_ids
            .lock()
            .expect("crash provider applied ids poisoned")
            .clone()
    }
}

#[async_trait::async_trait(?Send)]
impl Meter for CrashAfterProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        let fail_after = *self
            .fail_after
            .lock()
            .expect("crash provider fail_after poisoned");
        let mut applied = self
            .applied_ids
            .lock()
            .expect("crash provider applied ids poisoned");
        let mut accepted = 0;
        let mut deduped = 0;
        for (idx, event) in batch.iter().enumerate() {
            if fail_after == Some(idx) {
                return Err(ProviderError::Transport(
                    "injected mid-batch crash".to_string(),
                ));
            }
            if applied.contains(&event.event_id) {
                deduped += 1;
            } else {
                applied.push(event.event_id.clone());
                accepted += 1;
            }
        }
        Ok(IngestAck {
            accepted,
            deduped: Some(deduped),
        })
    }

    async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
        Ok(0)
    }
}

impl MeteringProvider for CrashAfterProvider {
    fn id(&self) -> &str {
        "crash_after"
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

async fn assert_dead_letter_row(
    client: &compio_postgres::Client,
    provider_id: &str,
    event_id: &str,
    reason: &str,
) {
    let rows = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.provider_dead_letter \
             WHERE provider_id = $1 AND event_id = $2 AND reason = $3",
            &[&provider_id, &event_id, &reason],
        )
        .await
        .expect("read provider_dead_letter");
    assert_eq!(
        rows[0].get::<_, i64>("n"),
        1,
        "one provider_dead_letter row for {provider_id}/{event_id}/{reason}"
    );
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
