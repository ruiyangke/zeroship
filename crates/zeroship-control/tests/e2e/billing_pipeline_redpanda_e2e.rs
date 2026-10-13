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

use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use compio_postgres::{connect, NoTls};
use uuid::Uuid;

use zeroship_control::cron::billing_reconcile::DEFAULT_SETTLE_WINDOW_SECS;
use zeroship_control::cron::spend_recompute::{recompute_usage_aggregates, SpendRecomputeConfig};
use zeroship_control::metering::current_period_start_unix;
use zeroship_control::Registry;
use zeroship_core::app_id::AppId;
use zeroship_metering::{Meter, UsageOutbox};
use zeroship_stream::{adapters, StreamConfig, StreamRegistry};

/// The clone this case owns.
///
/// The spend evaluator sweeps the whole fleet, so it reads every app the
/// database holds and needs the global pricing singleton; a sibling's app on a
/// plan that inherits the global FX would abort the sweep on a row this case
/// never seeded. A clone no sibling observes keeps the case self-sufficient.
fn db_url() -> String {
    crate::support::isolated_control_db()
}

/// The broker every Redpanda case of the run shares.
///
/// One broker per worktree run keeps the suite under the host's global
/// `fs.aio-max-nr`: Seastar refuses to start when several brokers boot at once.
fn brokers() -> String {
    zeroship_testkit::redpanda::broker().brokers()
}

/// The child test the broker's lifetime measurements run, by its full path in
/// this binary.
const CHILD_TEST: &str =
    "e2e::billing_pipeline_redpanda_e2e::child_joins_the_billing_broker_at_a_throwaway_scope";

/// Child side of the broker's lifetime measurements: join the broker recipe this
/// pipeline runs against at the throwaway scope on standard input.
#[test]
#[ignore = "spawned by a lifetime measurement as a child process, with its scope on stdin"]
fn child_joins_the_billing_broker_at_a_throwaway_scope() {
    let broker = zeroship_testkit::redpanda::Broker::join(&zeroship_testkit_server::lifetime::child_scope())
        .expect("join the broker recipe");
    assert!(
        broker.brokers().starts_with("127.0.0.1:"),
        "{}",
        broker.brokers()
    );
    zeroship_testkit_server::lifetime::report_container(broker.container_id());
}

#[test]
fn the_redpanda_broker_is_removed_when_its_process_ends() {
    zeroship_testkit_server::lifetime::assert_removed_after_the_child_exits(CHILD_TEST);
}

#[test]
fn the_redpanda_broker_is_removed_when_its_process_is_killed_while_starting() {
    zeroship_testkit_server::lifetime::assert_removed_after_a_kill_during_startup(CHILD_TEST);
}

async fn pg(url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(url, NoTls).await.expect("pg connect");
    crate::support::live::spawn(async move {
        let _ = conn.run().await;
    });
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
                runtime_limits_json, heap_limit_mb, spend_limit_default_cents) \
             VALUES ($1, 'e2e-test', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000}', 64, 100)",
            &[&plan_id, &fx_one_cent],
        )
        .await
        .expect("seed e2e plan");
    plan_id
}

async fn seed_priced_app(client: &compio_postgres::Client, plan_id: &str) -> AppId {
    let app_id = AppId::mint();
    let name = format!("e2e-probe-{}", Uuid::new_v4());
    let organization_id = crate::support::seed_organization(client).await;
    let project_id = crate::support::unowned_project_in(client, &organization_id).await;
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

/// The stream timeouts this case configures, named once so the case's budget
/// is derived from the same values the transports are built with.
const MESSAGE_TIMEOUT_MS: u64 = 10_000;
const PUBLISH_TIMEOUT_MS: u64 = 10_000;
const POLL_TIMEOUT_MS: u64 = 250;

fn redpanda_config(brokers: &str, topic: &str, group: &str) -> StreamConfig {
    StreamConfig::from(serde_json::json!({
        "brokers": brokers,
        "topic": topic,
        "group.id": group,
        "client_id": format!("zeroship-e2e-{}", Uuid::new_v4().simple()),
        "message_timeout_ms": MESSAGE_TIMEOUT_MS,
        "publish_timeout_ms": PUBLISH_TIMEOUT_MS,
        "poll_timeout_ms": POLL_TIMEOUT_MS,
        "auto_offset_reset": "earliest"
    }))
}

/// The longest the pipeline below may take, summed from the bounds its broker
/// steps carry: one publish, held to the publish timeout, and one recompute
/// cycle, whose rewind, end capture and scan are each held to the stall
/// timeout (`message_timeout_ms`), the scan's last blocking poll overrunning
/// it by at most one poll timeout. The database steps carry no configured
/// bound of their own and share what the broker steps leave unused.
const fn pipeline_budget() -> Duration {
    Duration::from_millis(PUBLISH_TIMEOUT_MS + 3 * MESSAGE_TIMEOUT_MS + POLL_TIMEOUT_MS)
}

fn set_phase(phase: &Mutex<&'static str>, name: &'static str) {
    *phase.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = name;
}

fn get_phase(phase: &Mutex<&'static str>) -> &'static str {
    *phase.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An OS-level backstop, independent of the async runtime this test awaits on.
///
/// Every rdkafka call this pipeline makes, and `BaseConsumer`/`BaseProducer`'s
/// `Drop`, is SYNCHRONOUS: it blocks the one OS thread the executor runs on
/// rather than suspending. A `compio::time::timeout` wrapped around the
/// pipeline cannot fire while one of those calls is stuck, because the
/// executor never regains control to notice its own timer expired - the
/// timeout only catches a future that genuinely suspends (a PG query, the
/// outbox's own stall-bounded wait) and stays stuck somewhere else. This
/// thread waits on its own `recv_timeout`, off that runtime entirely, and
/// `abort()`s the PROCESS if nothing signals it within the budget, so nextest
/// (one process per test) reports the hang immediately, named by whatever
/// phase was in flight - instead of the whole run riding out the test
/// runner's own, unnamed timeout.
///
/// Signalled on drop (win or panic) so an ordinary assertion failure, or a
/// stall the async timeout below already caught cleanly, does not leave this
/// thread waiting out the rest of the budget before a now-pointless `abort()`.
struct Watchdog {
    done: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Watchdog {
    fn start(budget: Duration, phase: Arc<Mutex<&'static str>>) -> Self {
        let (done, rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            if rx.recv_timeout(budget).is_err() {
                let stuck = get_phase(&phase);
                eprintln!(
                    "billing pipeline e2e watchdog: no completion signal within {budget:?}; \
                     aborting the process - stuck in phase: {stuck}"
                );
                std::process::abort();
            }
        });
        Self {
            done: Some(done),
            handle: Some(handle),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[compio::test(crate = "crate::support::live")]
async fn producer_to_redpanda_to_recompute_to_spend_block_end_to_end() {
    let budget = pipeline_budget();
    let phase = Arc::new(Mutex::new("joining the shared redpanda broker fixture"));

    let brokers = brokers();
    let url = db_url();

    // The watchdog starts only once both shared fixtures have returned.
    // `zeroship_testkit::redpanda::broker()` and the PostgreSQL fixture bound
    // their own boot and lease waits, which on a cold host that still has to
    // build an image or boot a server run for minutes - far longer than this
    // pipeline's own budget - so starting the watchdog's clock before those
    // calls return would abort a healthy run while it joins a fixture, on
    // exactly the host where this case most needs to pass.
    //
    // Its own budget carries one stall timeout of slack above the async
    // budget the pipeline below is held to. That slack is what makes the two
    // bounds agree on which hangs belong to which of them: a step that
    // SUSPENDS (a stalled PG query, the outbox's own stall-bounded wait) is
    // always caught by the smaller async budget first, which fails the
    // assertion below and, in unwinding, drops this watchdog - cancelling it
    // before its later deadline ever arrives. Only a step that blocks the
    // executor's one OS thread - where the async timeout itself never gets
    // polled, so nothing can reach it before that thread resumes - survives
    // long enough to reach this slacked deadline, and that is what the
    // watchdog's `abort()`, with the phase name, exists to catch.
    let watchdog_budget = budget + Duration::from_millis(MESSAGE_TIMEOUT_MS);
    let _watchdog = Watchdog::start(watchdog_budget, Arc::clone(&phase));

    let pipeline = async {
        set_phase(&phase, "connecting to PostgreSQL");
        let client = pg(&url).await;
        let registry = Registry::new(&url).await.expect("registry");
        let period = current_period_start_unix();
        set_phase(&phase, "seeding the priced app");
        let plan_id = seed_pricing(&client).await;
        let app = seed_priced_app(&client, &plan_id).await;

        let suffix = Uuid::new_v4().simple().to_string();
        let topic = format!("zeroship-usage-e2e-{suffix}");

        // -- 1. REAL producer: worker Meter accrues usage, drains to UsageEvents --
        let meter = Meter::with_source("worker-e2e");
        meter.increment(&app, "requests", 100);
        let events = meter.drain();
        assert_eq!(events.len(), 1, "one drained requests event");
        assert_eq!(events[0].subject.app, Some(app.clone()));
        assert_eq!(events[0].value, 100);

        // -- 2. REAL outbox publishes them to the REAL Redpanda broker --
        set_phase(&phase, "publishing the usage event through the outbox");
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
        assert!(published.is_clean(), "publish to redpanda failed: {published:?}");
        assert_eq!(published.published, 1, "one event acked by the broker");

        // -- 3. REAL enforcement recompute consumes the broker (fresh consumer =
        //       cold-start rewind+seek) into usage_aggregates --
        set_phase(&phase, "running one recompute cycle against the broker");
        let mut consumer_registry = StreamRegistry::default();
        adapters::register_builtin(&mut consumer_registry);
        let recompute_stream = consumer_registry
            .build(
                "redpanda",
                &redpanda_config(&brokers, &topic, &format!("e2e-recompute-{suffix}")),
            )
            .expect("redpanda recompute transport");
        assert_eq!(
            recompute_stream.stall_timeout(),
            Some(Duration::from_millis(MESSAGE_TIMEOUT_MS)),
            "the recompute's stall bound is the configured message timeout"
        );
        let cfg = SpendRecomputeConfig {
            interval: Duration::from_secs(1),
            settle_window: Duration::from_secs(DEFAULT_SETTLE_WINDOW_SECS),
            batch_max: 16,
            ..Default::default()
        };

        // ONE cycle. The outbox returned only after the broker acknowledged
        // the event, so the end the scan captures already covers it, and a
        // complete scan reads it however late the fetch lands. A cycle that
        // stalls instead returns its named error and fails here.
        let cycle = recompute_usage_aggregates(&registry, recompute_stream.as_ref(), period, &cfg)
            .await
            .expect("recompute cycle");
        assert_eq!(cycle.polled, 1, "the scan read the one published record: {cycle:?}");
        set_phase(&phase, "reading the usage snapshot");
        assert_eq!(
            usage_total(&client, &app).await,
            100,
            "enforcement recompute must land the producer's 100 requests in usage_aggregates"
        );

        // -- 4. REAL spend evaluator prices the snapshot -> Block (100 cents == limit) --
        set_phase(&phase, "sweeping spend state");
        let transitions = crate::support::sweep_spend(registry, &url)
            .await
            .expect("evaluate_all");
        assert!(
            transitions.iter().any(|t| t.app_id == app),
            "the app must transition spend state after recompute"
        );
        assert_eq!(
            spend_state(&client, &app).await,
            "block",
            "100 requests x 1 cent == the $1.00 spend limit -> Block at the gateway edge"
        );

        // Named explicitly: `BaseConsumer`/`BaseProducer::drop` blocks the
        // thread on the underlying librdkafka client teardown, the other half
        // of what the watchdog above exists to catch.
        set_phase(&phase, "dropping the broker clients");
        drop(outbox);
        drop(producer_stream);
        drop(recompute_stream);

        let _ = std::fs::remove_dir_all(&wal_dir);
    };
    let finished = compio::time::timeout(budget, Box::pin(pipeline)).await.is_ok();
    assert!(
        finished,
        "the billing pipeline did not finish within its {budget:?} async budget; it was {} \
         when the budget ran out",
        get_phase(&phase)
    );
}
