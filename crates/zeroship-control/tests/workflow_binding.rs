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
use zeroship_core::AppId;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};
use zeroship_workflow::store::pg::{PgStore, WorkflowTables};
use zeroship_workflow_v8::WorkflowBinding;

use crate::common;

const TEST_CONTROL_KEY: &str = "test-control-key";
const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("zs-wf-plugin-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    pg: Arc<compio_postgres::Client>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    _database: crate::workflow_postgres::Database,
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

async fn build_fixture(database: crate::workflow_postgres::Database, label: &str) -> Fixture {
    let db_url = database.url();
    let db_url = db_url.as_str();
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::plan_catalog::seed_plans(&registry)
        .await
        .expect("seed builtin plans");
    // The DW-24 rollout gate is `apps.workflows_enabled AND plans.workflows_allowed`
    // (crates/zeroship-control/src/workflow_rollout.rs), and both columns default to
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
            service_auth: std::sync::Arc::new(
                zeroship_core::service_peers::ServiceAuth::unconfigured(),
            ),
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
            worker_urls: Vec::new(),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            worker_enrolment: zeroship_control::worker_enrolment::EnrolmentEnvelope::closed(),
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
            mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
            pairwise_salt: [0u8; 32],
            projected_charge_cache: Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        }),
        pg: control_pg,
        blob_root,
        deploy_tmp_dir,
        _database: database,
    }
}

async fn seed_app(fx: &Fixture, workflows: &[&str]) -> AppId {
    let app_id = AppId::mint();
    let app_name = format!("wf-plugin-{}", Uuid::new_v4().simple());
    // This case is about the V8 binding, not about who owns the app.
    let project = common::unowned_project(fx.pg.as_ref()).await;
    fx.pg
        .execute(
            "INSERT INTO zeroship.apps \
                 (id, name, plan_id, workflows_enabled, project_id, organization_id) \
             SELECT $1, $2, $3, true, p.id, p.organization_id \
               FROM zeroship.projects p WHERE p.id = $4",
            &[
                &app_id.as_str(),
                &app_name,
                &zeroship_control::plan_catalog::free_plan_id(),
                &project,
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
            &[
                &deploy_id,
                &app_id.as_str(),
                &format!("hash-{deploy_id}"),
                &manifest,
            ],
        )
        .await
        .expect("insert deploy");
    app_id
}

fn wf_sql(app_id: &AppId, sql: &str) -> String {
    let tables = WorkflowTables::for_app_id(app_id);
    sql.replace("zeroship.workflow_runs", &tables.runs)
        .replace("zeroship.workflow_steps", &tables.steps)
        .replace("zeroship.workflow_signals", &tables.signals)
        .replace("zeroship.workflow_subscriptions", &tables.subscriptions)
        .replace("zeroship.workflow_blobs", &tables.blobs)
}

fn modules(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: source.to_string(),
    }]
}

async fn run_workflow_app(control_url: String, app_id: &AppId, source: &str) -> (u16, String) {
    init_v8();
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.as_str().to_owned());
    let plugin: Arc<dyn NativePlugin> =
        Arc::new(WorkflowBinding::new(control_url, TEST_CONTROL_KEY));
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

#[compio::test]
async fn v8_binding_round_trips_through_the_control_instance_api() {
    let fx = build_fixture(crate::workflow_postgres::Database::new(), "round-trip").await;
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
    let (status, body) = run_workflow_app(control_url, &app_id, source).await;
    assert_eq!(status, 200, "body: {body}");
    let value: Value = serde_json::from_str(&body).expect("body json");
    assert_eq!(value["ok"], true, "body: {body}");
    let run_id = value["runId"].as_str().expect("run id");

    let run = fx
        .pg
        .query_one(
            &wf_sql(
                &app_id,
                "SELECT state FROM zeroship.workflow_runs WHERE id = $1 AND app_id = $2",
            ),
            &[&run_id, &app_id.as_str()],
        )
        .await
        .expect("run row");
    assert_eq!(run.get::<_, String>("state"), "queued");

    let signals = fx
        .pg
        .query_one(
            &wf_sql(
                &app_id,
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

#[compio::test]
async fn native_output_reads_preserve_bytes_and_reject_another_apps_run() {
    use sha2::{Digest, Sha256};
    use zeroship_workflow::{
        app_scoped_token, HttpWorkflowBackend, WorkflowBackend, WorkflowClientConfig,
    };

    let fx = build_fixture(crate::workflow_postgres::Database::new(), "output-read").await;
    let app = seed_app(&fx, &["Checkout"]).await;
    let other = seed_app(&fx, &["Checkout"]).await;
    let control = ControlServer::start(Arc::clone(&fx.state));
    let backend = HttpWorkflowBackend::new(WorkflowClientConfig::new(
        &control.base,
        app.as_str().to_owned(),
        app_scoped_token(TEST_CONTROL_KEY, app.as_str()),
    ));
    let started = backend
        .start("Checkout".into(), json!({"input":{}}))
        .await
        .unwrap();
    let run = started["id"].as_str().unwrap();
    let bytes = vec![0, 255, 128, 10];
    let hash = format!("{:x}", Sha256::digest(&bytes));
    fx.state
        .workflow_blob_store
        .put_blob(&hash, &bytes)
        .await
        .unwrap();
    fx.pg.execute(
        &wf_sql(&app, "INSERT INTO zeroship.workflow_steps \
         (run_id, ordinal, name, name_occurrence, kind, state, output_kind, output_hash, output_size, output_content_type, batch_id) \
         VALUES ($1, 0, 'payload', 0, 'run', 'completed', 'blob', $2, $3, 'application/octet-stream', 'batch_output')"),
        &[&run, &hash, &(bytes.len() as i64)],
    ).await.unwrap();
    let source = format!(
        r#"
        export default {{ async fetch(_request, env) {{
          try {{
            const bytes = await env.workflows.Checkout.get({run:?}).readStepOutput("payload", 0);
            return Response.json({{ bytes: [...bytes], typed: bytes instanceof Uint8Array }});
          }} catch (error) {{ return Response.json({{code: error.code, message: error.message}}, {{status: 400}}); }}
        }} }};
    "#
    );
    let (status, body) = run_workflow_app(control.base.clone(), &app, &source).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"bytes":bytes,"typed":true})
    );
    let (status, body) = run_workflow_app(control.base.clone(), &other, &source).await;
    assert_eq!(status, 400, "another app read the saved output: {body}");
    let error: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["code"], "workflow_http_error");
    assert!(error["message"].as_str().unwrap().contains("HTTP 404"));
    drop(control);
    drop(fx);
    common::drain_pg().await;
}
