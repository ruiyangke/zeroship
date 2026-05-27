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

use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    api, auth_service, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
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
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let auth = auth_service::AuthService::new(db_url, "test-jwt-secret-please-ignore")
        .await
        .expect("auth service");
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let vfs: Arc<dyn BundleStore + Send + Sync> = Arc::new(
        LocalFs::new(blob_root.join("legacy-bundles")).expect("vfs"),
    );

    Fixture {
        state: Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            auth,
            google_oauth: None,
            vfs,
            blob_store,
            control_key: SecretString::new("test-control-key".to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            worker_urls,
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            insecure_dev: false,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            oidc_rp: None,
            auth_pg: None,
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
    let Some(db_url) = db_url() else {
        eprintln!("[app_logs_http_test] CONTROL_TEST_DB not set - skipping");
        return;
    };

    let app_id = Uuid::new_v4();
    let worker = test::server(async || {
        web::App::new().service(
            web::resource("/logs/{app_id}").route(web::get().to(worker_logs)),
        )
    })
    .await;
    let fixture = build_test_state(&db_url, vec![worker.url("/")]).await;

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
        .get(format!("/api/apps/{app_id}/logs"))
        .header("authorization", format!("Bearer {TEST_MASTER_KEY}"))
        .send()
        .await
        .expect("control response");
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.body().await.expect("body");
    let lines: Vec<String> = serde_json::from_slice(&body).expect("logs json");
    eprintln!("[app_logs_http_test] captured logs: {lines:?}");
    assert_eq!(lines, vec![format!("b2-control-route-log {app_id}")]);
}
