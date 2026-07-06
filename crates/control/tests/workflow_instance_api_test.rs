//! HTTP-level tests for the durable-workflows instance API.
//!
//! Requires `CONTROL_TEST_DB` pointing at a migrated disposable Postgres
//! database. This matches the rest of the control integration suite: no DB
//! means the tests skip without failing local `cargo test`.

#![allow(clippy::await_holding_lock, clippy::future_not_send)]

mod common;

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

const TEST_CONTROL_KEY: &str = "test-control-key";
const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

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
    common::ensure_builtin_plans(&registry).await;
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let control_pg = Arc::new(pg(db_url).await);

    Fixture {
        state: Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            blob_store,
            control_key: SecretString::new(TEST_CONTROL_KEY.to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "http://127.0.0.1:9".to_string(),
            gateway_url: "http://127.0.0.1:9".to_string(),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            insecure_dev: false,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::clone(&control_pg),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(zeroship_authn::PatIssuer::dev_insecure()),
            auth_provider: zeroship_control::platform_auth_provider(
                "https://auth.zeroship.test/oauth2",
                Some("http://127.0.0.1:9/oauth2/.well-known/jwks.json".to_string()),
            ),
            logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
            metering_provider: zeroship_control::metering::provider::build_provider(
                &zeroship_control::metering::provider::MeteringProviderConfig::native(),
            )
            .expect("native provider builds"),
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

async fn seed_app(fx: &Fixture, label: &str, workflows: &[&str]) -> (Uuid, String) {
    let app_id = Uuid::new_v4();
    let app_name = format!("wf-api-{label}-{}", Uuid::new_v4().simple());
    fx.pg
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, 'test-api-key', 'test-api-key-hash')",
            &[
                &app_id,
                &app_name,
                &zeroship_control::bootstrap_console::free_plan_id(),
            ],
        )
        .await
        .expect("insert app");
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

async fn seed_app_without_deploy(fx: &Fixture, label: &str) -> Uuid {
    let app_id = Uuid::new_v4();
    fx.pg
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, 'test-api-key', 'test-api-key-hash')",
            &[
                &app_id,
                &format!("wf-api-nodeploy-{label}-{}", Uuid::new_v4().simple()),
                &zeroship_control::bootstrap_console::free_plan_id(),
            ],
        )
        .await
        .expect("insert app without deploy");
    app_id
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
async fn create_conflicts_status_and_cross_app_isolation() {
    let Some(db_url) = db_url() else {
        eprintln!("skipping workflow_instance_api_test (no CONTROL_TEST_DB)");
        return;
    };
    let fx = build_fixture(&db_url, "create").await;
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
            "SELECT state FROM zeroship.workflow_runs WHERE id = $1 AND app_id = $2",
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

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::get().uri(&format!("/internal/workflows/runs/{first_run}")),
            app_b,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "app B cannot read app A run");

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{first_run}/signal"))
                .set_json(&json!({ "type": "approved", "payload": { "ok": true } })),
            app_b,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "app B cannot signal app A run");

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{first_run}/cancel")),
            app_b,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "app B cannot cancel app A run");

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
            "SELECT id, state, dedup_key \
               FROM zeroship.workflow_runs \
              WHERE id = ANY($1) \
              ORDER BY id",
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

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/MissingWorkflow/runs")
                .set_json(&json!({ "input": {} })),
            app_a,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri("/internal/workflows/Checkout/runs")
                .set_json(&json!({ "input": {} })),
            app_no_deploy,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[compio::test]
async fn signal_writes_row_and_pulls_matching_wait_wake_at() {
    let Some(db_url) = db_url() else {
        eprintln!("skipping workflow_instance_api_test (no CONTROL_TEST_DB)");
        return;
    };
    let fx = build_fixture(&db_url, "signal").await;
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
            "UPDATE zeroship.workflow_runs \
                SET state = 'waiting', wake_at = $1, waiting_step_key = 'wait:0:approved:approved:60000' \
              WHERE id = $2",
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
            "SELECT state, wake_at \
               FROM zeroship.workflow_runs \
              WHERE id = $1 AND app_id = $2",
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
            "SELECT type, payload, origin, delivery \
               FROM zeroship.workflow_signals \
              WHERE run_id = $1",
            &[&run_id],
        )
        .await
        .expect("signal row");
    assert_eq!(signal.get::<_, String>("type"), "approved");
    assert_eq!(signal.get::<_, Value>("payload"), json!({ "by": "usr_test" }));
    assert_eq!(signal.get::<_, String>("origin"), "app");
    assert_eq!(signal.get::<_, String>("delivery"), "direct");
}

#[compio::test]
async fn pause_resume_cancel_transitions_preserve_wake_and_discard_claim() {
    let Some(db_url) = db_url() else {
        eprintln!("skipping workflow_instance_api_test (no CONTROL_TEST_DB)");
        return;
    };
    let fx = build_fixture(&db_url, "control").await;
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
            "UPDATE zeroship.workflow_runs \
                SET state = 'sleeping', wake_at = $1, waiting_step_key = 'sleep:0:cooldown' \
              WHERE id = $2",
            &[&future_wake, &run_id],
        )
        .await
        .expect("make sleeping");

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
            "SELECT state, wake_at FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("paused row");
    assert_eq!(row.get::<_, String>("state"), "paused");
    let paused_wake: DateTime<Utc> = row.get("wake_at");
    assert!(
        paused_wake
            .signed_duration_since(future_wake)
            .num_milliseconds()
            .abs()
            <= 1,
        "pause should preserve wake_at, got {paused_wake:?} vs {future_wake:?}"
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
            "SELECT state, wake_at FROM zeroship.workflow_runs WHERE id = $1",
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
            "UPDATE zeroship.workflow_runs \
                SET state = 'running', claimed_by = 'owner-a', dispatch_nonce = 'wfd_claim', \
                    lease_expires = $2 \
              WHERE id = $1",
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
            "SELECT state, wake_at, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("cancelled row");
    assert_eq!(row.get::<_, String>("state"), "cancelled");
    assert_eq!(row.get::<_, Option<DateTime<Utc>>>("wake_at"), None);
    assert_eq!(row.get::<_, Option<String>>("claimed_by"), None);
    assert_eq!(row.get::<_, Option<String>>("dispatch_nonce"), None);
}
