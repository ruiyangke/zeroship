#![allow(clippy::await_holding_lock, clippy::future_not_send)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use compio_postgres::{connect, NoTls};
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore, LocalWorkflowBlobStore, WorkflowBlobStore};
use zeroship_control::{
    workflow_instance_api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};
use zeroship_plugin_workflow::{
    app_scoped_token, is_excluded_workflow_property, WorkflowClientConfig, WorkflowHttpMethod,
    WorkflowPlugin,
};
use zeroship_plugin_workflow::store::pg::{PgStore, WorkflowTables};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

use crate::common;

const TEST_CONTROL_KEY: &str = "test-control-key";
const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> Option<String> {
    zeroship_core::config::test_database_url_opt()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-wf-plugin-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    pg: Arc<compio_postgres::Client>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

struct ControlServer {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ControlServer {
    fn start(state: Arc<AppState>) -> Self {
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("workflow-plugin-control-test")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = test::server(move || {
                        let state = Arc::clone(&state);
                        async move {
                            web::App::new()
                                .state(state)
                                .configure(workflow_instance_api::configure)
                        }
                    })
                    .await;
                    let addr = server.addr();
                    started_tx
                        .send(format!("http://localhost:{}", addr.port()))
                        .expect("send control test server addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let base = started_rx.recv().expect("control test server starts");
        Self {
            base,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn pg(db_url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

async fn build_fixture(db_url: &str, label: &str) -> Fixture {
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::plan_catalog::seed_plans(&registry)
        .await
        .expect("seed builtin plans");
    // The DW-24 rollout gate is `apps.workflows_enabled AND plans.workflows_allowed`
    // (crates/control/src/workflow_rollout.rs), and both columns default to
    // false. Without this the control instance API answers every
    // `env.workflows.*` call with 403 "workflows are not enabled for this app or
    // plan". `workflow_instance_api_test` enables the same two flags in its own
    // fixture; this suite was never run by any CI job, so it never had to.
    {
        let setup_pg = pg(db_url).await;
        setup_pg
            .execute(
                "UPDATE zeroship.plans SET workflows_allowed = true WHERE id IN ($1, $2, $3)",
                &[
                    &zeroship_control::plan_catalog::free_plan_id(),
                    &zeroship_control::plan_catalog::pro_plan_id(),
                    &zeroship_control::plan_catalog::unlimited_plan_id(),
                ],
            )
            .await
            .expect("enable workflows on the built-in plans");
    }
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn WorkflowBlobStore> =
        Arc::new(LocalWorkflowBlobStore::new(blob_root.clone()).expect("workflow blob store"));
    let control_pg = Arc::new(pg(db_url).await);

    Fixture {
        state: Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            blob_store,
            workflow_blob_store,
            control_key: SecretString::new(TEST_CONTROL_KEY.to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "http://127.0.0.1:9".to_string(),
            gateway_url: "http://127.0.0.1:9".to_string(),
            migrate_server_url: "http://127.0.0.1:9".to_string(),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::clone(&control_pg),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            auth_provider: zeroship_control::platform_auth_provider(
                "https://auth.zeroship.test/oauth2",
                Some(common::platform_jwks_url()),
            ),
            // No platform deploy-token mint here: that is control's OUTBOUND
            // destination for the device flow, and no fixture below drives one.
            provider_registry: zeroship_control::metering::provider::builtin_registry(),
            billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
            billing_stream: None,
            tax_provider: zeroship_control::tax::build_tax_provider(
                &zeroship_control::tax::TaxProviderConfig::native(),
            )
            .expect("native tax provider builds"),
            notifier: Arc::new(zeroship_control::notify::RecordingNotifier::new()),
            pairwise_salt: [0u8; 32],
            projected_charge_cache: Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        }),
        pg: control_pg,
        blob_root,
        deploy_tmp_dir,
    }
}

async fn seed_app(fx: &Fixture, workflows: &[&str]) -> Uuid {
    let app_id = Uuid::new_v4();
    let app_name = format!("wf-plugin-{}", Uuid::new_v4().simple());
    fx.pg
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash, workflows_enabled) \
             VALUES ($1, $2, $3, 'test-api-key', 'test-api-key-hash', true)",
            &[
                &app_id,
                &app_name,
                &zeroship_control::plan_catalog::free_plan_id(),
            ],
        )
        .await
        .expect("insert app");
    common::provision_app_workflow_schema(fx.pg.as_ref(), &app_id).await;
    PgStore::provision(fx.pg.as_ref(), &app_id)
        .await
        .expect("provision workflow journal");
    let deploy_id = format!("dep_{}", Uuid::new_v4().simple());
    let manifest = json!({
        "version": 1,
        "workflows": workflows,
        "assets": {},
        "runtime_assets": {},
        "asset_version": 0,
        "sourcemaps": {},
        "metadata": { "built_at": "2026-07-05T00:00:00Z" },
    })
    .to_string();
    fx.pg
        .execute(
            "INSERT INTO zeroship.app_deploys (id, app_id, deploy_hash, manifest_json, activated_at) \
             VALUES ($1, $2, $3, $4, now())",
            &[&deploy_id, &app_id, &format!("hash-{deploy_id}"), &manifest],
        )
        .await
        .expect("insert deploy");
    app_id
}

fn wf_sql(app_id: Uuid, sql: &str) -> String {
    let tables = WorkflowTables::for_app_id(&app_id);
    sql.replace("zeroship.workflow_runs", &tables.runs)
        .replace("zeroship.workflow_steps", &tables.steps)
        .replace("zeroship.workflow_signals", &tables.signals)
        .replace(
            "zeroship.workflow_subscriptions",
            &tables.subscriptions,
        )
        .replace("zeroship.workflow_blobs", &tables.blobs)
}

fn modules(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: source.to_string(),
    }]
}

async fn run_workflow_app(control_url: String, app_id: Uuid, source: &str) -> (u16, String) {
    init_v8();
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.to_string());
    let plugin: Arc<dyn NativePlugin> =
        Arc::new(WorkflowPlugin::new(control_url, TEST_CONTROL_KEY));
    let runtime = Runtime::builder()
        .modules(modules(source))
        .env_vars(env_vars)
        .plugins(vec![plugin])
        .build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        FetchOutcome::Pending { rx, cancel: _ } => {
            let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .expect("workflow plugin fetch timed out")
                .expect("workflow plugin pending dispatch failed");
            match settled {
                SettledFetch::Response { status, body, .. } => {
                    (status, String::from_utf8_lossy(&body).into_owned())
                }
                other => {
                    let name = match other {
                        SettledFetch::Stream { .. } => "Stream",
                        SettledFetch::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                        SettledFetch::Response { .. } => unreachable!(),
                    };
                    panic!("workflow plugin: expected response, got {name}");
                }
            }
        }
        FetchOutcome::Stream { .. } => panic!("workflow plugin: unexpected stream"),
        FetchOutcome::WebSocketUpgrade { .. } => {
            panic!("workflow plugin: unexpected websocket upgrade")
        }
    }
}

async fn run_dev_workflow_app(
    db_path: &std::path::Path,
    app_id: Uuid,
    source: &str,
) -> (u16, String) {
    init_v8();
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.to_string());
    let plugin: Arc<dyn NativePlugin> = Arc::new(
        WorkflowPlugin::dev_sqlite(db_path, modules(source), env_vars.clone(), Vec::new())
            .expect("dev workflow plugin"),
    );
    let runtime = Runtime::builder()
        .modules(modules(source))
        .env_vars(env_vars)
        .plugins(vec![plugin])
        .build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        FetchOutcome::Pending { rx, cancel: _ } => {
            let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .expect("workflow dev fetch timed out")
                .expect("workflow dev pending dispatch failed");
            match settled {
                SettledFetch::Response { status, body, .. } => {
                    (status, String::from_utf8_lossy(&body).into_owned())
                }
                other => {
                    let name = match other {
                        SettledFetch::Stream { .. } => "Stream",
                        SettledFetch::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                        SettledFetch::Response { .. } => unreachable!(),
                    };
                    panic!("workflow dev: expected response, got {name}");
                }
            }
        }
        FetchOutcome::Stream { .. } => panic!("workflow dev: unexpected stream"),
        FetchOutcome::WebSocketUpgrade { .. } => {
            panic!("workflow dev: unexpected websocket upgrade")
        }
    }
}

#[test]
fn derives_the_app_scoped_token_with_the_shared_core_helper() {
    let app_id = Uuid::new_v4().to_string();
    let token = app_scoped_token(TEST_CONTROL_KEY, &app_id);
    assert_eq!(
        token,
        zeroship_core::auth::derive_app_scoped_control_token(TEST_CONTROL_KEY, &app_id)
    );
    assert!(zeroship_core::auth::validate_app_scoped_control_token(
        &token,
        TEST_CONTROL_KEY,
        &app_id
    ));
    assert!(!zeroship_core::auth::validate_app_scoped_control_token(
        &token,
        "wrong-key",
        &app_id
    ));
}

#[test]
fn builds_authenticated_workflow_instance_requests() {
    let cfg = WorkflowClientConfig::new(
        "http://control.test/",
        "app_123",
        app_scoped_token(TEST_CONTROL_KEY, "app_123"),
    );
    let start = zeroship_plugin_workflow::client::build_start_request(
        &cfg,
        "Checkout/Final",
        json!({ "input": { "orderId": 42 }, "key": "cart-42" }),
    )
    .expect("start request");
    assert_eq!(start.method, WorkflowHttpMethod::Post);
    assert_eq!(
        start.url,
        "http://control.test/internal/workflows/Checkout%2FFinal/runs"
    );
    assert_eq!(start.app_id_header, "app_123");
    assert_eq!(start.authorization, format!("Bearer {}", cfg.token()));
    assert_eq!(
        serde_json::from_slice::<Value>(start.body.as_deref().expect("body")).unwrap(),
        json!({ "input": { "orderId": 42 }, "key": "cart-42" })
    );

    let status =
        zeroship_plugin_workflow::client::build_get_status_request(&cfg, "run_abc/def")
            .expect("status request");
    assert_eq!(status.method, WorkflowHttpMethod::Get);
    assert_eq!(
        status.url,
        "http://control.test/internal/workflows/runs/run_abc%2Fdef"
    );
    assert!(status.body.is_none());

    let restart = zeroship_plugin_workflow::client::build_restart_request(
        &cfg,
        "run_abc/def",
        json!({ "from": { "name": "charge" } }),
    )
    .expect("restart request");
    assert_eq!(restart.method, WorkflowHttpMethod::Post);
    assert_eq!(
        restart.url,
        "http://control.test/internal/workflows/runs/run_abc%2Fdef/restart"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(restart.body.as_deref().expect("body")).unwrap(),
        json!({ "from": { "name": "charge" } })
    );
}

#[test]
fn getter_exclusion_list_matches_the_binding_contract() {
    for name in [
        "",
        "then",
        "toJSON",
        "inspect",
        "constructor",
        "prototype",
        "__proto__",
        "__defineGetter__",
        "__defineSetter__",
        "__lookupGetter__",
        "__lookupSetter__",
        "hasOwnProperty",
        "isPrototypeOf",
        "propertyIsEnumerable",
        "toLocaleString",
        "toString",
        "valueOf",
    ] {
        assert!(is_excluded_workflow_property(name), "{name} must be excluded");
    }
    assert!(!is_excluded_workflow_property("Checkout"));
}

#[compio::test]
async fn dev_sqlite_engine_runs_sleep_signal_core_loop_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("workflows.sqlite");
    let app_id = Uuid::new_v4();
    let source = r#"
        const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

        export class LocalApproval {
          async run(trigger, step) {
            const first = await step.run("record", () => ({ orderId: trigger.input.orderId }));
            await step.sleep("cooldown", "20ms");
            const signal = await step.waitForSignal("approved", { type: "approved" });
            return { first, approved: signal.payload };
          }
        }

        export default {
          async fetch(_req, env) {
            const run = await env.workflows.LocalApproval.start({ input: { orderId: "ord_1" } });
            let beforeSignal = null;
            for (let i = 0; i < 30; i += 1) {
              beforeSignal = await run.status();
              if (beforeSignal.state === "waiting") break;
              await delay(20);
            }
            const signal = await run.signal({ type: "approved", payload: { by: "local" } });
            let finalStatus = null;
            for (let i = 0; i < 30; i += 1) {
              finalStatus = await run.status();
              if (finalStatus.state === "completed") break;
              await delay(20);
            }
            return Response.json({ runId: run.id, beforeSignal, signal, finalStatus });
          }
        };
    "#;

    let (status, body) = run_dev_workflow_app(&db_path, app_id, source).await;
    assert_eq!(status, 200, "body: {body}");
    let value: Value = serde_json::from_str(&body).expect("body json");
    assert_eq!(value["beforeSignal"]["state"], "waiting", "body: {body}");
    assert_eq!(value["finalStatus"]["state"], "completed", "body: {body}");
    assert_eq!(
        value["finalStatus"]["output"],
        json!({
            "first": { "orderId": "ord_1" },
            "approved": { "by": "local" }
        }),
        "body: {body}"
    );
    let run_id = value["runId"].as_str().expect("run id");
    let sqlite = rusqlite::Connection::open(&db_path).expect("open workflow db");
    let step_count: i64 = sqlite
        .query_row(
            "SELECT COUNT(*) FROM workflow_steps WHERE run_id = ?1 AND name = 'record' AND kind = 'run'",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("step count");
    assert_eq!(step_count, 1, "step.run must execute exactly once");
    let completed_wait: i64 = sqlite
        .query_row(
            "SELECT COUNT(*) FROM workflow_steps WHERE run_id = ?1 AND name = 'approved' AND kind = 'wait_signal' AND state = 'completed'",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("wait count");
    assert_eq!(completed_wait, 1, "signal wait should complete once");
}

#[compio::test]
async fn v8_binding_getter_exclusions_are_undefined() {
    let source = r#"
        export default {
          fetch(_req, env) {
            return Response.json({
              thenIsUndefined: env.workflows.then === undefined,
              toJSONIsUndefined: env.workflows.toJSON === undefined,
              toStringIsUndefined: env.workflows.toString === undefined,
              hasOwnPropertyIsUndefined: env.workflows.hasOwnProperty === undefined,
              symbolIsUndefined: env.workflows[Symbol.toStringTag] === undefined,
              handleHasStart: typeof env.workflows.Checkout.start === "function",
              handleHasGet: typeof env.workflows.Checkout.get === "function"
            });
          }
        };
    "#;
    let (status, body) = run_workflow_app(
        "http://127.0.0.1:9".to_string(),
        Uuid::new_v4(),
        source,
    )
    .await;
    assert_eq!(status, 200, "body: {body}");
    let value: Value = serde_json::from_str(&body).expect("body json");
    assert_eq!(value["thenIsUndefined"], true, "body: {body}");
    assert_eq!(value["toJSONIsUndefined"], true, "body: {body}");
    assert_eq!(value["toStringIsUndefined"], true, "body: {body}");
    assert_eq!(value["hasOwnPropertyIsUndefined"], true, "body: {body}");
    assert_eq!(value["symbolIsUndefined"], true, "body: {body}");
    assert_eq!(value["handleHasStart"], true, "body: {body}");
    assert_eq!(value["handleHasGet"], true, "body: {body}");
}

#[compio::test]
async fn v8_binding_round_trips_through_the_control_instance_api() {
    let Some(db_url) = db_url() else {
        zeroship_test_support::skip("skipping workflow plugin control round-trip (no test database)");
        return;
    };
    let fx = build_fixture(&db_url, "round-trip").await;
    let app_id = seed_app(&fx, &["Checkout"]).await;
    let control = ControlServer::start(Arc::clone(&fx.state));
    let control_url = control.base.clone();
    let source = r#"
        export default {
          async fetch(_req, env) {
            const failures = [];
            function check(ok, step, value) {
              if (!ok) failures.push({ step, value });
            }
            check(env.workflows.then === undefined, "then", typeof env.workflows.then);
            check(env.workflows[Symbol.toStringTag] === undefined, "symbol", String(env.workflows[Symbol.toStringTag]));
            check(env.workflows.toJSON === undefined, "toJSON", typeof env.workflows.toJSON);
            check(env.workflows.toString === undefined, "inherited", typeof env.workflows.toString);
            const run = await env.workflows.Checkout.start({ input: { orderId: "ord_1" } });
            check(typeof run.id === "string" && run.id.startsWith("run_"), "run.id", run.id);
            const first = await run.status();
            check(first.state === "queued", "status.queued", first);
            const signal = await run.signal({ type: "approved", payload: { by: "tester" } });
            check(typeof signal.id === "string" && signal.id.startsWith("sig_"), "signal.id", signal.id);
            const cancel = await run.cancel();
            check(cancel.state === "cancelled", "cancel.state", cancel);
            const finalStatus = await env.workflows.Checkout.get(run.id).status();
            check(finalStatus.state === "cancelled", "status.cancelled", finalStatus);
            const restarted = await run.restart();
            check(restarted.id === run.id, "restart.sameRun", restarted.id);
            const restartStatus = await restarted.status();
            check(restartStatus.state === "queued", "restart.status", restartStatus);
            return Response.json({
              ok: failures.length === 0,
              failures,
              runId: run.id,
              signalId: signal.id,
              first,
              cancel,
              finalStatus,
              restartStatus
            }, { status: failures.length === 0 ? 200 : 500 });
          }
        };
    "#;
    let (status, body) = run_workflow_app(control_url, app_id, source).await;
    assert_eq!(status, 200, "body: {body}");
    let value: Value = serde_json::from_str(&body).expect("body json");
    assert_eq!(value["ok"], true, "body: {body}");
    let run_id = value["runId"].as_str().expect("run id");

    let run = fx
        .pg
        .query_one(
            &wf_sql(
                app_id,
                "SELECT state FROM zeroship.workflow_runs WHERE id = $1 AND app_id = $2",
            ),
            &[&run_id, &app_id],
        )
        .await
        .expect("run row");
    assert_eq!(run.get::<_, String>("state"), "queued");

    let signals = fx
        .pg
        .query_one(
            &wf_sql(
                app_id,
                "SELECT count(*)::int4 AS count \
               FROM zeroship.workflow_signals \
              WHERE run_id = $1 AND type = 'approved'",
            ),
            &[&run_id],
        )
        .await
        .expect("signal count");
    assert_eq!(signals.get::<_, i32>("count"), 1);

    // Teardown: the control-server thread and the fixture both hold
    // connections, and locals are dropped only after the body returns - by
    // which point the runtime is gone and the sockets can no longer be
    // closed. Drop them explicitly, then wait for the close to land.
    drop(control);
    drop(fx);
    common::drain_pg().await;
}
