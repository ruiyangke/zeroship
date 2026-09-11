//! HTTP-level tests for the durable-workflows instance API.
//!
//! Each fixture uses an owned, migrated Testcontainers database.

#![allow(clippy::await_holding_lock, clippy::future_not_send)]

use crate::common;

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use compio_postgres::{connect, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    workflow_instance_api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};
use zeroship_workflow::store::pg::{PgStore, WorkflowTables};
use zeroship_workflow_scheduler::WorkflowSchedulerStore;

const TEST_CONTROL_KEY: &str = "test-control-key";
const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-wf-instance-{label}-{}",
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
    _database: crate::workflow_postgres::Database,
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
    common::ensure_builtin_plans(&registry).await;
    WorkflowSchedulerStore::new(db_url.to_string())
        .provision()
        .await
        .expect("provision workflow scheduler store");
    let setup_pg = pg(db_url).await;
    setup_pg
        .execute(
            "UPDATE zeroship.plans SET workflows_allowed = true \
              WHERE id IN ($1, $2, $3)",
            &[
                &zeroship_control::plan_catalog::free_plan_id(),
                &zeroship_control::plan_catalog::pro_plan_id(),
                &zeroship_control::plan_catalog::unlimited_plan_id(),
            ],
        )
        .await
        .expect("enable workflow built-in plans for test");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );
    let control_pg = Arc::new(pg(db_url).await);

    Fixture {
        state: Arc::new(AppState {
            service_auth: std::sync::Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured()),
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

async fn seed_app(fx: &Fixture, label: &str, workflows: &[&str]) -> (Uuid, String) {
    seed_app_on_plan(
        fx,
        label,
        workflows,
        &zeroship_control::plan_catalog::free_plan_id(),
    )
    .await
}

async fn seed_app_on_plan(
    fx: &Fixture,
    label: &str,
    workflows: &[&str],
    plan_id: &str,
) -> (Uuid, String) {
    let app_id = Uuid::new_v4();
    let app_name = format!("wf-api-{label}-{}", Uuid::new_v4().simple());
    // These cases are about workflow admission, not about who owns the app, so
    // the app just needs a home. See `common::unowned_project`.
    let project = common::unowned_project(fx.pg.as_ref()).await;
    fx.pg
        .execute(
            "INSERT INTO zeroship.apps \
                 (id, name, plan_id, workflows_enabled, project_id, organization_id) \
             SELECT $1, $2, $3, true, p.id, p.organization_id \
               FROM zeroship.projects p WHERE p.id = $4",
            &[&app_id, &app_name, &plan_id, &project],
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
        .expect("insert app deploy");
    (app_id, deploy_id)
}

async fn seed_workflow_cap_plan(fx: &Fixture, label: &str, run_cap: i64, app_cap: i64) -> String {
    let plan_id = format!("pln_wf_api_cap_{}_{}", label, Uuid::new_v4().simple());
    let runtime = json!({
        "cpu_limit_ms": 50,
        "wall_timeout_ms": 5000,
        "heap_limit_mb": 64,
        "workflow_journal_max_bytes": run_cap,
        "workflow_app_journal_max_bytes": app_cap,
    });
    let net = json!({
        "max_sockets": 4,
        "egress_ceiling_bytes": 10485760,
    });
    fx.pg
        .execute(
            "INSERT INTO zeroship.plans \
                (id, name, runtime_limits_json, net_policy_limits_json, spend_limit_default_cents, workflows_allowed) \
             VALUES ($1, $2, $3, $4, 0, true)",
            &[&plan_id, &format!("wf-api-cap-{label}"), &runtime, &net],
        )
        .await
        .expect("insert workflow cap plan");
    plan_id
}

async fn pg_json_size(fx: &Fixture, value: &Value) -> i64 {
    fx.pg
        .query_one(
            "SELECT pg_column_size($1::jsonb)::bigint AS bytes",
            &[value],
        )
        .await
        .expect("pg_column_size jsonb")
        .get("bytes")
}

async fn seed_app_without_deploy(fx: &Fixture, label: &str) -> Uuid {
    let app_id = Uuid::new_v4();
    let project = common::unowned_project(fx.pg.as_ref()).await;
    fx.pg
        .execute(
            "INSERT INTO zeroship.apps \
                 (id, name, plan_id, workflows_enabled, project_id, organization_id) \
             SELECT $1, $2, $3, true, p.id, p.organization_id \
               FROM zeroship.projects p WHERE p.id = $4",
            &[
                &app_id,
                &format!("wf-api-nodeploy-{label}-{}", Uuid::new_v4().simple()),
                &zeroship_control::plan_catalog::free_plan_id(),
                &project,
            ],
        )
        .await
        .expect("insert app without deploy");
    common::provision_app_workflow_schema(fx.pg.as_ref(), &app_id).await;
    PgStore::provision(fx.pg.as_ref(), &app_id)
        .await
        .expect("provision workflow journal");
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

fn authed(req: test::TestRequest, app_id: Uuid) -> test::TestRequest {
    let token =
        zeroship_core::auth::derive_app_scoped_control_token(TEST_CONTROL_KEY, &app_id.to_string());
    req.header("authorization", format!("Bearer {token}"))
        .header(workflow_instance_api::APP_ID_HEADER, app_id.to_string())
}

fn run_id(value: &Value) -> String {
    value["id"].as_str().expect("response id").to_string()
}

#[compio::test]
async fn workflow_routes_reject_missing_auth() {
    let fx = build_fixture(crate::workflow_postgres::Database::new(), "auth-required").await;
    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    let status = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/internal/workflows/runs")
            .header(
                workflow_instance_api::APP_ID_HEADER,
                Uuid::new_v4().to_string(),
            )
            .to_request(),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "app-scoped workflow routes require a derived control token"
    );

    let status = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/internal/workflows/signals/fanout/tick")
            .to_request(),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "workflow ingress routes require the control key"
    );

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn archived_app_rejects_new_runs_until_restore() {
    let fx = build_fixture(crate::workflow_postgres::Database::new(), "archived-admission").await;
    let (app_id, _) = seed_app(&fx, "archived-admission", &["Checkout"]).await;
    fx.state
        .registry
        .archive_app(&app_id)
        .await
        .expect("archive workflow app")
        .expect("workflow app exists");
    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": { "orderId": 1 } })),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::FORBIDDEN);
    let run_count: i64 = fx
        .pg
        .query_one(
            &wf_sql(
                app_id,
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_runs WHERE app_id = $1",
            ),
            &[&app_id],
        )
        .await
        .expect("count archived app runs")
        .get("n");
    assert_eq!(run_count, 0, "archive must reject before journal mutation");

    fx.state
        .registry
        .unarchive_app(&app_id)
        .await
        .expect("restore workflow app")
        .expect("workflow app exists");
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": { "orderId": 2 } })),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::CREATED);

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn create_conflicts_status_and_cross_app_isolation() {
    let fx = build_fixture(crate::workflow_postgres::Database::new(), "create").await;
    let (app_a, _) = seed_app(&fx, "a", &["Checkout", "OtherWorkflow"]).await;
    let (app_b, _) = seed_app(&fx, "b", &["Checkout"]).await;
    let app_no_deploy = seed_app_without_deploy(&fx, "create").await;

    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": { "orderId": 1 } })),
            app_a,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let first_run = run_id(&body);
    assert!(first_run.starts_with("run_"));

    let row = fx
        .pg
        .query_one(
            &wf_sql(
                app_a,
                "SELECT state FROM zeroship.workflow_runs WHERE id = $1 AND app_id = $2",
            ),
            &[&first_run, &app_a],
        )
        .await
        .expect("created run row");
    assert_eq!(row.get::<_, String>("state"), "queued");

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::get().uri(&format!("/internal/workflows/runs/{first_run}")),
            app_a,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(status["state"], "queued");

    // Status only for these three: each `resp` below is shadowed by the next
    // `let resp = ...` before ever being consumed, so a retained `WebResponse`
    // would keep the app state's Postgres client alive past teardown.
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::get().uri(&format!("/internal/workflows/runs/{first_run}")),
            app_b,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::NOT_FOUND, "app B cannot read app A run");

    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{first_run}/signal"))
                .set_json(&json!({ "type": "approved", "payload": { "ok": true } })),
            app_b,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::NOT_FOUND, "app B cannot signal app A run");

    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{first_run}/cancel")),
            app_b,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::NOT_FOUND, "app B cannot cancel app A run");

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": { "orderId": 2 }, "key": "order:2" })),
            app_a,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let keyed: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let keyed_run = run_id(&keyed);

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({
                    "input": { "orderId": 999 },
                    "key": "order:2",
                    "onConflict": "join"
                })),
            app_a,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let joined: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(run_id(&joined), keyed_run, "join returns incumbent run id");

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/OtherWorkflow/runs")
                .set_json(&json!({ "input": {}, "key": "order:2", "onConflict": "reject" })),
            app_a,
        )
        .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "same dedupe key on a different workflow is independent"
    );
    let other: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_ne!(run_id(&other), keyed_run);

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({
                    "input": { "orderId": 3 },
                    "key": "order:2",
                    "onConflict": "reject"
                })),
            app_a,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let conflict: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(conflict["error"], "RunConflict");

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({
                    "input": { "orderId": 4 },
                    "key": "order:2",
                    "onConflict": { "policy": "replace" }
                })),
            app_a,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let replaced: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let replacement_run = run_id(&replaced);
    assert_ne!(replacement_run, keyed_run, "replace creates a new run");
    let rows = fx
        .pg
        .query(
            &wf_sql(
                app_a,
                "SELECT id, state, dedup_key \
               FROM zeroship.workflow_runs \
              WHERE id = ANY($1) \
              ORDER BY id",
            ),
            &[&vec![keyed_run.clone(), replacement_run.clone()]],
        )
        .await
        .expect("replace rows");
    let incumbent = rows
        .iter()
        .find(|row| row.get::<_, String>("id") == keyed_run)
        .expect("incumbent row");
    assert_eq!(incumbent.get::<_, String>("state"), "cancelled");
    assert_eq!(incumbent.get::<_, Option<String>>("dedup_key"), None);
    let replacement = rows
        .iter()
        .find(|row| row.get::<_, String>("id") == replacement_run)
        .expect("replacement row");
    assert_eq!(replacement.get::<_, String>("state"), "queued");
    assert_eq!(
        replacement.get::<_, Option<String>>("dedup_key").as_deref(),
        Some("order:2")
    );

    // Status only: both calls below are the last uses of `resp` in this test,
    // and a retained `WebResponse` keeps the app state - and its Postgres
    // client - alive past the teardown at the end of this test.
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/MissingWorkflow/runs")
                .set_json(&json!({ "input": {} })),
            app_a,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": {} })),
            app_no_deploy,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Teardown: the service and the fixture both hold connections, and locals
    // are dropped only after the body returns - by which point the runtime is
    // gone and the sockets can no longer be closed. Drop them explicitly, then
    // wait for the close to land.
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn terminal_runs_release_their_app_start_key_without_changing_history() {
    let fx = build_fixture(crate::workflow_postgres::Database::new(), "terminal-key").await;
    let (app_id, _) = seed_app(&fx, "terminal-key", &["Checkout"]).await;
    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;
    for terminal in ["completed", "failed", "cancelled"] {
        let response = test::call_service(
            &app,
            authed(
                test::TestRequest::post()
                    .uri("/internal/workflows/Checkout/runs")
                    .set_json(&json!({"input":{}, "key":terminal})),
                app_id,
            )
            .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = serde_json::from_slice(&test::read_body(response).await).unwrap();
        let previous = run_id(&body);
        fx.pg.execute(&wf_sql(app_id,
            "UPDATE zeroship.workflow_runs SET state = $2, output = 'true', terminal_at = now(), wake_at = NULL WHERE id = $1"),
            &[&previous, &terminal]).await.unwrap();
        let response = test::call_service(
            &app,
            authed(
                test::TestRequest::post()
                    .uri("/internal/workflows/Checkout/runs")
                    .set_json(&json!({"input":{}, "key":terminal, "onConflict":"reject"})),
                app_id,
            )
            .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = serde_json::from_slice(&test::read_body(response).await).unwrap();
        assert_ne!(run_id(&body), previous);
        let row = fx
            .pg
            .query_one(
                &wf_sql(
                    app_id,
                    "SELECT state, output, dedup_key FROM zeroship.workflow_runs WHERE id = $1",
                ),
                &[&previous],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>("state"), terminal);
        assert_eq!(row.get::<_, Value>("output"), json!(true));
        assert_eq!(row.get::<_, Option<String>>("dedup_key"), None);
    }
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn signal_writes_row_and_pulls_matching_wait_wake_at() {
    let fx = build_fixture(crate::workflow_postgres::Database::new(), "signal").await;
    let (app_id, _) = seed_app(&fx, "signal", &["Checkout"]).await;
    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": { "orderId": 5 } })),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let run_id = run_id(&created);
    let future_wake = Utc::now() + ChronoDuration::hours(1);
    fx.pg
        .execute(
            &wf_sql(
                app_id,
                "UPDATE zeroship.workflow_runs \
                SET state = 'waiting', wake_at = $1, waiting_step_key = 'wait:0:approved:approved:60000' \
              WHERE id = $2",
            ),
            &[&future_wake, &run_id],
        )
        .await
        .expect("park run waiting");

    let before = Utc::now();
    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{run_id}/signal"))
                .set_json(&json!({ "type": "approved", "payload": { "by": "usr_test" } })),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert!(body["id"].as_str().unwrap().starts_with("sig_"));

    let row = fx
        .pg
        .query_one(
            &wf_sql(
                app_id,
                "SELECT state, wake_at \
               FROM zeroship.workflow_runs \
              WHERE id = $1 AND app_id = $2",
            ),
            &[&run_id, &app_id],
        )
        .await
        .expect("read signaled run");
    assert_eq!(row.get::<_, String>("state"), "waiting");
    let wake_at: DateTime<Utc> = row.get("wake_at");
    assert!(
        wake_at >= before - ChronoDuration::milliseconds(100)
            && wake_at <= Utc::now() + ChronoDuration::seconds(1),
        "wake_at should be pulled to now, got {wake_at:?}"
    );

    let signal = fx
        .pg
        .query_one(
            &wf_sql(
                app_id,
                "SELECT type, payload, origin, delivery \
               FROM zeroship.workflow_signals \
              WHERE run_id = $1",
            ),
            &[&run_id],
        )
        .await
        .expect("signal row");
    assert_eq!(signal.get::<_, String>("type"), "approved");
    assert_eq!(signal.get::<_, Value>("payload"), json!({ "by": "usr_test" }));
    assert_eq!(signal.get::<_, String>("origin"), "app");
    assert_eq!(signal.get::<_, String>("delivery"), "direct");

    let oversized_input = json!({ "blob": "x".repeat(256) });
    let oversized_input_bytes = pg_json_size(&fx, &oversized_input).await;
    let create_cap_plan = seed_workflow_cap_plan(
        &fx,
        "create-cap",
        oversized_input_bytes + 1024,
        oversized_input_bytes - 1,
    )
    .await;
    let (create_cap_app, _) =
        seed_app_on_plan(&fx, "create-cap", &["Checkout"], &create_cap_plan).await;
    // Status only: no read_body follows, so a retained `WebResponse` would
    // keep the app state - and its Postgres client - alive past teardown.
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": oversized_input })),
            create_cap_app,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "per-app aggregate journal cap should 429 create"
    );
    let rows = fx
        .pg
        .query(
            &wf_sql(
                create_cap_app,
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_runs WHERE app_id = $1",
            ),
            &[&create_cap_app],
        )
        .await
        .expect("count capped create runs");
    assert_eq!(rows[0].get::<_, i64>("n"), 0);

    let capped_input = json!({ "seed": true });
    let capped_payload = json!({ "payload": "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy" });
    let capped_input_bytes = pg_json_size(&fx, &capped_input).await;
    let capped_payload_bytes = pg_json_size(&fx, &capped_payload).await;
    let signal_cap_plan = seed_workflow_cap_plan(
        &fx,
        "signal-cap",
        1_000_000,
        capped_input_bytes + capped_payload_bytes - 1,
    )
    .await;
    let (signal_cap_app, _) =
        seed_app_on_plan(&fx, "signal-cap", &["Checkout"], &signal_cap_plan).await;
    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": capped_input })),
            signal_cap_app,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let capped_created: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let capped_run_id = capped_created["id"]
        .as_str()
        .expect("capped create response id")
        .to_string();
    // Status only: this is the last use of `resp` in this test, and a
    // retained `WebResponse` would keep the app state's Postgres client
    // alive past the teardown below.
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{capped_run_id}/signal"))
                .set_json(&json!({ "type": "approved", "payload": capped_payload })),
            signal_cap_app,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "per-app aggregate journal cap should 429 signal"
    );
    let rows = fx
        .pg
        .query(
            &wf_sql(
                signal_cap_app,
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_signals WHERE run_id = $1",
            ),
            &[&capped_run_id],
        )
        .await
        .expect("count capped signal rows");
    assert_eq!(rows[0].get::<_, i64>("n"), 0);

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn pause_resume_cancel_transitions_preserve_wake_and_discard_claim() {
    let fx = build_fixture(crate::workflow_postgres::Database::new(), "control").await;
    let (app_id, _) = seed_app(&fx, "control", &["Checkout"]).await;
    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": { "orderId": 6 } })),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let run_id = run_id(&created);
    let future_wake = Utc::now() + ChronoDuration::minutes(30);
    fx.pg
        .execute(
            &wf_sql(
                app_id,
                "UPDATE zeroship.workflow_runs \
                SET state = 'sleeping', wake_at = $1, waiting_step_key = 'sleep:0:cooldown' \
              WHERE id = $2",
            ),
            &[&future_wake, &run_id],
        )
        .await
        .expect("make sleeping");
    fx.pg
        .execute(
            &wf_sql(
                app_id,
                "INSERT INTO zeroship.workflow_steps \
                    (run_id, ordinal, name, name_occurrence, kind, state, wake_at, batch_id, batch_width) \
                 VALUES ($1, 0, 'cooldown', 0, 'sleep', 'running', $2, 'wfd_pause_resume', 1)",
            ),
            &[&run_id, &future_wake],
        )
        .await
        .expect("insert sleeping frontier step");

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/pause")),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["state"], "paused");
    let row = fx
        .pg
        .query_one(
            &wf_sql(
                app_id,
                "SELECT state, wake_at FROM zeroship.workflow_runs WHERE id = $1",
            ),
            &[&run_id],
        )
        .await
        .expect("paused row");
    assert_eq!(row.get::<_, String>("state"), "paused");
    assert!(
        row.get::<_, Option<DateTime<Utc>>>("wake_at").is_none(),
        "pause should clear wake_at"
    );

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/resume")),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["state"], "sleeping");
    let row = fx
        .pg
        .query_one(
            &wf_sql(
                app_id,
                "SELECT state, wake_at FROM zeroship.workflow_runs WHERE id = $1",
            ),
            &[&run_id],
        )
        .await
        .expect("resumed row");
    assert_eq!(row.get::<_, String>("state"), "sleeping");
    let resumed_wake: DateTime<Utc> = row.get("wake_at");
    assert!(
        resumed_wake
            .signed_duration_since(future_wake)
            .num_milliseconds()
            .abs()
            <= 1,
        "resume should preserve wake_at, got {resumed_wake:?} vs {future_wake:?}"
    );

    let lease_expires = Utc::now() + ChronoDuration::minutes(5);
    fx.pg
        .execute(
            &wf_sql(
                app_id,
                "UPDATE zeroship.workflow_runs \
                SET state = 'running', claimed_by = 'owner-a', dispatch_nonce = 'wfd_claim', \
                    lease_expires = $2 \
              WHERE id = $1",
            ),
            &[&run_id, &lease_expires],
        )
        .await
        .expect("make claimed running");

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/cancel")),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["state"], "cancelled");
    let row = fx
        .pg
        .query_one(
            &wf_sql(
                app_id,
                "SELECT state, wake_at, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs WHERE id = $1",
            ),
            &[&run_id],
        )
        .await
        .expect("cancelled row");
    assert_eq!(row.get::<_, String>("state"), "cancelled");
    assert_eq!(row.get::<_, Option<DateTime<Utc>>>("wake_at"), None);
    assert_eq!(row.get::<_, Option<String>>("claimed_by"), None);
    assert_eq!(row.get::<_, Option<String>>("dispatch_nonce"), None);

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Injects a scheduler-store timer-registration failure for one app.
///
/// A `BEFORE INSERT` trigger on `zeroship.workflow_scheduler_timers` raises
/// whenever the inserted row's `app_id` is listed in a sentinel table, so the
/// failure is scoped to the app under test and every other registration in the
/// binary is untouched. This is the same class of failure a dead store
/// connection produces, and it is the only failure the handler can hit after
/// its journal writes have already succeeded.
async fn install_timer_registration_failpoint(fx: &Fixture) {
    fx.pg
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS zeroship.zs_test_timer_insert_fails \
                 (app_id uuid PRIMARY KEY); \
             CREATE OR REPLACE FUNCTION zeroship.zs_test_fail_timer_insert() \
                 RETURNS trigger AS $fp$ \
             BEGIN \
                 IF EXISTS (SELECT 1 FROM zeroship.zs_test_timer_insert_fails f \
                             WHERE f.app_id = NEW.app_id) THEN \
                     RAISE EXCEPTION 'injected workflow scheduler timer registration failure'; \
                 END IF; \
                 RETURN NEW; \
             END; \
             $fp$ LANGUAGE plpgsql; \
             DROP TRIGGER IF EXISTS zs_test_fail_timer_insert ON zeroship.workflow_scheduler_timers; \
             CREATE TRIGGER zs_test_fail_timer_insert \
                 BEFORE INSERT ON zeroship.workflow_scheduler_timers \
                 FOR EACH ROW EXECUTE FUNCTION zeroship.zs_test_fail_timer_insert();",
        )
        .await
        .expect("install timer registration failpoint");
}

async fn remove_timer_registration_failpoint(fx: &Fixture) {
    fx.pg
        .batch_execute(
            "DROP TRIGGER IF EXISTS zs_test_fail_timer_insert ON zeroship.workflow_scheduler_timers; \
             DROP FUNCTION IF EXISTS zeroship.zs_test_fail_timer_insert(); \
             DROP TABLE IF EXISTS zeroship.zs_test_timer_insert_fails;",
        )
        .await
        .expect("remove timer registration failpoint");
}

async fn arm_timer_registration_failure(fx: &Fixture, app_id: Uuid) {
    fx.pg
        .execute(
            "INSERT INTO zeroship.zs_test_timer_insert_fails (app_id) \
             VALUES ($1) ON CONFLICT DO NOTHING",
            &[&app_id],
        )
        .await
        .expect("arm timer registration failure");
}

async fn disarm_timer_registration_failure(fx: &Fixture, app_id: Uuid) {
    fx.pg
        .execute(
            "DELETE FROM zeroship.zs_test_timer_insert_fails WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("disarm timer registration failure");
}

async fn count_runs(fx: &Fixture, app_id: Uuid, workflow_name: &str) -> i64 {
    fx.pg
        .query_one(
            &wf_sql(
                app_id,
                "SELECT count(*)::bigint AS n FROM zeroship.workflow_runs \
                  WHERE app_id = $1 AND workflow_name = $2",
            ),
            &[&app_id, &workflow_name],
        )
        .await
        .expect("count runs")
        .get("n")
}

/// A start whose timer registration fails must leave NO run behind, so a client
/// retry of the resulting 500 creates exactly one run and the workflow executes
/// once.
///
/// Before the fix the handler committed the run and only then registered the
/// timer: the failure returned 500 over a durable `queued` run with
/// `wake_at = now()`, which `run_inflight_reaper` adopts and executes. Unkeyed
/// starts carry no dedup key, so the client's retry inserted a second run and a
/// non-idempotent workflow ran twice.
///
/// What this test does NOT catch: it drives the failure only through the
/// timers INSERT, so a registration path that fails before reaching that
/// statement (for example the run-tables lookup) is not exercised; and it says
/// nothing about the keyed (`key` + `onConflict`) start path, which is already
/// protected by the unique index on `(app_id, workflow_name, dedup_key)`.
#[compio::test]
async fn failed_timer_registration_leaves_no_run_so_a_retry_starts_exactly_one() {
    let fx = build_fixture(crate::workflow_postgres::Database::new(), "timerfail").await;
    let (app_id, _) = seed_app(&fx, "timerfail", &["Charge"]).await;
    install_timer_registration_failpoint(&fx).await;
    arm_timer_registration_failure(&fx, app_id).await;

    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    // Unkeyed start: no dedup key, so nothing but transactional atomicity can
    // stop a retry from creating a second run.
    let start = || {
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Charge/runs")
                .set_json(&json!({ "input": { "amountCents": 4200 } })),
            app_id,
        )
        .to_request()
    };

    let resp = test::call_service(&app, start()).await;
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a start whose timer registration fails must not report success"
    );
    let runs_after_failed_start = count_runs(&fx, app_id, "Charge").await;

    // The client retries the 500. Registration now succeeds.
    disarm_timer_registration_failure(&fx, app_id).await;
    let resp = test::call_service(&app, start()).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let retried_run = run_id(&body);
    let runs_after_retry = count_runs(&fx, app_id, "Charge").await;

    // Both counts in one assertion so a failure reports the whole story: how
    // many runs the failed start left behind, and how many exist after the
    // retry. The second number is also the control for the first — the same
    // query against the same table DOES see the row a successful start writes,
    // so a zero from it means zero rows and not an unreachable table.
    assert_eq!(
        (runs_after_failed_start, runs_after_retry),
        (0, 1),
        "a failed start must leave no run, and the unkeyed retry must leave exactly one \
         (got {runs_after_failed_start} after the 500, {runs_after_retry} after the retry)"
    );
    let timers: i64 = fx
        .pg
        .query_one(
            "SELECT count(*)::bigint AS n FROM zeroship.workflow_scheduler_timers WHERE run_id = $1",
            &[&retried_run],
        )
        .await
        .expect("count timers")
        .get("n");
    assert_eq!(timers, 1, "the successful start must register its timer");

    remove_timer_registration_failpoint(&fx).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}
