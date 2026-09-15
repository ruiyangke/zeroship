//! HTTP regression for `GET /api/apps/{id}/logs`.
//!
//! This drives the control-plane route over a running ntex server and
//! makes it fetch log lines from a running worker-shaped HTTP endpoint.
//! Before B2 the control router did not register this path, so the same
//! request returned 404.

use std::path::PathBuf;
use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> String {
    crate::common::require_control_db()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-app-logs-test-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_test_state(db_url: &str, worker_urls: Vec<String>) -> Fixture {
    let blob_root = tmpdir("blob");
    let deploy_tmp_dir = tmpdir("deploy");

    let registry = Registry::new(db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );

    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();
    let control_pg = Arc::new(control_pg_client);

    Fixture {
        state: Arc::new(AppState {
            service_auth: std::sync::Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured()),
            registry,
            env_store,
            stripe_store,
            blob_store,
            workflow_blob_store,
            control_key: SecretString::new("test-control-key".to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".to_string(),
            gateway_url: "http://127.0.0.1:9".to_string(),
            worker_urls,
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            worker_enrolment: zeroship_control::worker_join::EnrolmentEnvelope::closed(),
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg,
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            auth_provider: zeroship_control::platform_auth_provider("https://auth.zeroship.test/oauth2", Some(common::platform_jwks_url())),
        // No platform deploy-token mint here: that is control's OUTBOUND
        // destination for the device flow, and no fixture below drives one.
        provider_registry: zeroship_control::metering::provider::builtin_registry(),
        billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
        billing_stream: None,
            tax_provider: zeroship_control::tax::build_tax_provider(
                &zeroship_control::tax::TaxProviderConfig::native(),
            )
            .expect("native tax provider builds"),
            notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
            mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
            pairwise_salt: [0u8; 32],
            projected_charge_cache: std::sync::Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        }),
        blob_root,
        deploy_tmp_dir,
    }
}

async fn worker_logs(path: web::types::Path<String>) -> web::HttpResponse {
    web::HttpResponse::Ok().json(&vec![format!("b2-control-route-log {}", path.into_inner())])
}

#[ntex::test]
async fn app_logs_route_proxies_worker_lines() {
    let db_url = db_url();

    let worker = test::server(async || {
        web::App::new().service(
            web::resource("/logs/{app_id}").route(web::get().to(worker_logs)),
        )
    })
    .await;
    let fixture = build_test_state(&db_url, vec![worker.url("/")]).await;
    // A REAL app the caller owns. This used to be a bare `Uuid::new_v4()` with
    // no app row and no membership, which reached the worker proxy only because
    // the caller held the deleted universal-allow platform role.
    let pat = common::authz_fixture::seeded_principal(&fixture.state).await;
    // `create_app` validates its plan against the catalog, which this database
    // only has once something seeds it.
    common::ensure_builtin_plans(&fixture.state.registry).await;
    let app_id = fixture
        .state
        .registry
        .create_app(
            &format!("logs-{}", &Uuid::new_v4().simple().to_string()[..10]),
            &zeroship_control::plan_catalog::free_plan_id(),
            &pat.user_id,
            None,
            None,
        )
        .await
        .expect("create app")
        .id;

    let state = fixture.state.clone();
    let control = test::server(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).service(
                web::resource("/api/apps/{id}/logs")
                    .route(web::get().to(api::get_app_logs)),
            )
        }
    })
    .await;

    let response = control
        .get(format!("/api/apps/{}/logs", app_id.as_str()))
        .send()
        .await
        .expect("unauthenticated control response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = control
        .get(format!("/api/apps/{}/logs", app_id.as_str()))
        .header("authorization", pat.bearer())
        .send()
        .await
        .expect("control response");
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.body().await.expect("body");
    let lines: Vec<String> = serde_json::from_slice(&body).expect("logs json");
    eprintln!("[app_logs_http_test] captured logs: {lines:?}");
    // The stub echoes the path segment control sent, and control addresses the
    // worker's log endpoint by the TYPED app id - the worker parses that
    // segment with `AppId::parse` and refuses a uuid rendering. So the echo is
    // the typed rendering, not `app_id`'s own Display.
    assert_eq!(
        lines,
        vec![format!("b2-control-route-log {}", app_id.as_str())]
    );
    pat.cleanup(&fixture.state).await;

    // Teardown: `control` runs its app factory (holding a cloned `Arc<AppState>`)
    // on its own dedicated system/thread, and `fixture` owns the state's real
    // Postgres connection. Neither is released until both are dropped - and
    // locals are dropped only after the body returns, by which point this
    // test's compio runtime is gone and the socket can no longer be closed.
    // Drop them explicitly, then wait for the close to land.
    drop(control);
    drop(worker);
    drop(fixture);
    common::drain_pg().await;
}
