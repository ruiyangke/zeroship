//! End-to-end billing/enforcement pipeline over a real Redpanda broker.
//!
//! This is the faithful wired path — no shims for the components under test:
//!
//!   real Meter (worker producer) ──drain──► real UsageOutbox ──publish──►
//!     real Redpanda ──recompute (rewind+poll)──► usage_aggregates (live PG)
//!       ──► real SpendEngine.evaluate_all ──► app_spend_state (Block)
//!
//! It proves the producer, the durable stream, the enforcement recompute (which
//! `rewind()`s a fresh consumer every cycle — the cold-start seek path), and the
//! spend evaluator work together against a real broker and real Postgres, rather
//! than each in isolation with a fake stream.

use std::net::{Ipv4Addr, TcpListener};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use compio_postgres::{connect, NoTls};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};
use uuid::Uuid;

use zeroship_control::cron::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS;
use zeroship_control::cron::spend_recompute::{recompute_usage_aggregates, SpendRecomputeConfig};
use zeroship_control::metering::current_period_start_unix;
use zeroship_control::spend::SpendEngine;
use zeroship_control::Registry;
use zeroship_core::app_id::AppId;
use zeroship_metering::{Meter, UsageOutbox};
use zeroship_stream::{adapters, StreamConfig, StreamRegistry};

use crate::common;

fn db_url() -> String {
    crate::common::require_control_db()
}

struct Redpanda {
    _container: Container<GenericImage>,
    brokers: String,
}

impl Redpanda {
    fn start() -> Self {
        let port = available_port();
        let advertised = format!("external://127.0.0.1:{port}");
        let container = GenericImage::new("docker.redpanda.com/redpandadata/redpanda", "v26.2.2")
            .with_wait_for(WaitFor::message_on_stderr("Successfully started Redpanda!"))
            .with_mapped_port(port, 19092.tcp())
            .with_cmd([
                "redpanda".to_owned(),
                "start".to_owned(),
                "--overprovisioned".to_owned(),
                "--smp".to_owned(),
                "1".to_owned(),
                "--memory".to_owned(),
                "512M".to_owned(),
                "--reserve-memory".to_owned(),
                "0M".to_owned(),
                "--node-id".to_owned(),
                "0".to_owned(),
                "--check=false".to_owned(),
                "--kafka-addr".to_owned(),
                "external://0.0.0.0:19092".to_owned(),
                "--advertise-kafka-addr".to_owned(),
                advertised,
                "--set".to_owned(),
                "redpanda.auto_create_topics_enabled=true".to_owned(),
            ])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .expect("control tests require Docker and Redpanda");
        Self {
            _container: container,
            brokers: format!("127.0.0.1:{port}"),
        }
    }
}

fn available_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("bind an available Redpanda port")
        .local_addr()
        .expect("Redpanda listener address")
        .port()
}

static REDPANDA: OnceLock<Redpanda> = OnceLock::new();

fn brokers() -> String {
    REDPANDA.get_or_init(Redpanda::start).brokers.to_owned()
}

async fn pg(url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

/// Seed the `requests` metric (weight 1) + a plan priced at 1¢/unit with a $1.00
/// spend limit, so 100 requests → 100¢ → exactly the limit → Block.
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
    let plan_id = format!("pln_e2e_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'e2e-test', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100)",
            &[&plan_id, &fx_one_cent],
        )
        .await
        .expect("seed e2e plan");
    plan_id
}

async fn seed_priced_app(client: &compio_postgres::Client, plan_id: &str) -> AppId {
    let app_id = AppId::mint();
    let name = format!("e2e-probe-{}", Uuid::new_v4());
    let organization_id = common::seed_organization(client).await;
    let project_id = common::unowned_project_in(client, &organization_id).await;
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                &app_id.as_str(),
                &name,
                &plan_id,
                &project_id,
                &organization_id,
            ],
        )
        .await
        .expect("seed fixture app");
    app_id
}

async fn usage_total(client: &compio_postgres::Client, app: &AppId) -> i64 {
    // The app is freshly created, so it has at most this run's single period.
    let rows = client
        .query(
            "SELECT COALESCE(SUM(total), 0)::bigint AS n FROM zeroship.usage_aggregates \
             WHERE app_id = $1",
            &[&app.as_str()],
        )
        .await
        .expect("query usage_aggregates");
    rows[0].get("n")
}

async fn spend_state(client: &compio_postgres::Client, app: &AppId) -> String {
    let rows = client
        .query(
            "SELECT state FROM zeroship.app_spend_state WHERE app_id = $1",
            &[&app.as_str()],
        )
        .await
        .expect("query app_spend_state");
    rows.first()
        .map(|r| r.get::<_, String>("state"))
        .unwrap_or_else(|| "<none>".to_string())
}

fn redpanda_config(brokers: &str, topic: &str, group: &str) -> StreamConfig {
    StreamConfig::from(serde_json::json!({
        "brokers": brokers,
        "topic": topic,
        "group.id": group,
        "client_id": format!("zeroship-e2e-{}", Uuid::new_v4().simple()),
        "message_timeout_ms": 10000,
        "publish_timeout_ms": 10000,
        "poll_timeout_ms": 250,
        "auto_offset_reset": "earliest"
    }))
}

#[compio::test]
async fn producer_to_redpanda_to_recompute_to_spend_block_end_to_end() {
    let brokers = brokers();

    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let period = current_period_start_unix();
    let plan_id = seed_pricing(&client).await;
    let app = seed_priced_app(&client, &plan_id).await;

    let suffix = Uuid::new_v4().simple().to_string();
    let topic = format!("zeroship-usage-e2e-{suffix}");

    // ── 1. REAL producer: worker Meter accrues usage, drains to UsageEvents ──
    let meter = Meter::with_source("worker-e2e");
    meter.increment(&app, "requests", 100);
    let events = meter.drain();
    assert_eq!(events.len(), 1, "one drained requests event");
    assert_eq!(events[0].subject.app, Some(app.clone()));
    assert_eq!(events[0].value, 100);

    // ── 2. REAL outbox publishes them to the REAL Redpanda broker ──
    let mut producer_registry = StreamRegistry::default();
    adapters::register_builtin(&mut producer_registry);
    let producer_stream = producer_registry
        .build(
            "redpanda",
            &redpanda_config(&brokers, &topic, &format!("e2e-producer-{suffix}")),
        )
        .expect("redpanda producer transport");
    let wal_dir = std::env::temp_dir().join(format!("zeroship-e2e-wal-{suffix}"));
    std::fs::create_dir_all(&wal_dir).expect("mk wal dir");
    let outbox = UsageOutbox::new(
        Arc::clone(&producer_stream),
        topic.clone(),
        wal_dir.join("usage-outbox.redb"),
    )
    .expect("usage outbox");
    let published = outbox.publish_events(&events).await;
    assert!(
        published.failed.is_empty(),
        "publish to redpanda failed: {:?}",
        published.failed
    );
    assert_eq!(published.published, 1, "one event acked by the broker");

    // ── 3. REAL enforcement recompute consumes the broker (fresh consumer =
    //       cold-start rewind+seek) into usage_aggregates ──
    let mut consumer_registry = StreamRegistry::default();
    adapters::register_builtin(&mut consumer_registry);
    let recompute_stream = consumer_registry
        .build(
            "redpanda",
            &redpanda_config(&brokers, &topic, &format!("e2e-recompute-{suffix}")),
        )
        .expect("redpanda recompute transport");
    let cfg = SpendRecomputeConfig {
        interval: std::time::Duration::from_secs(1),
        settle_window: std::time::Duration::from_secs(DEFAULT_SETTLE_WINDOW_SECS),
        batch_max: 16,
    };

    // Poll-drive the recompute until the published event lands (broker fetch is
    // async); bounded so a genuine failure still terminates.
    let mut total = 0i64;
    for _ in 0..20 {
        let cycle = recompute_usage_aggregates(&registry, recompute_stream.as_ref(), period, &cfg)
            .await
            .expect("recompute cycle");
        let _ = cycle;
        total = usage_total(&client, &app).await;
        if total >= 100 {
            break;
        }
    }
    assert_eq!(
        total, 100,
        "enforcement recompute must land the producer's 100 requests in usage_aggregates"
    );

    // ── 4. REAL spend evaluator prices the snapshot → Block (100¢ == limit) ──
    let transitions = SpendEngine::new(registry)
        .evaluate_all()
        .await
        .expect("evaluate_all");
    assert!(
        transitions.iter().any(|t| t.app_id == app),
        "the app must transition spend state after recompute"
    );
    assert_eq!(
        spend_state(&client, &app).await,
        "block",
        "100 requests × 1¢ == the $1.00 spend limit → Block at the gateway edge"
    );

    let _ = std::fs::remove_dir_all(&wal_dir);

    // Teardown: `client` holds this test's Postgres connection (the `registry`
    // connection was already consumed into the `SpendEngine` temporary above,
    // which drops - and asks its connection to close - at the end of that
    // statement), and locals are dropped only after the body returns - by which
    // point the runtime is gone and the socket can no longer be closed. Drop it
    // explicitly, then wait for the close to land.
    drop(client);
    common::drain_pg().await;
}
