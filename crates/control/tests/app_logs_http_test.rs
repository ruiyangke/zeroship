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
    api, oidc_rp, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};

mod common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
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
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let vfs: Arc<dyn BundleStore + Send + Sync> = Arc::new(
        LocalFs::new(blob_root.join("legacy-bundles")).expect("vfs"),
    );

    let oidc_rp = Arc::new(oidc_rp::ConsoleOidcRp::new(
        "http://localhost:4444",
        "console.zeroship.ai",
        "test-oidc-secret".to_string(),
        b"test-stash-key".to_vec(),
    ));
    let (auth_pg_client, auth_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("auth-pg connect");
    compio::runtime::spawn(async move {
        let _ = auth_pg_conn.run().await;
    })
    .detach();
    let auth_pg = Arc::new(auth_pg_client);

    Fixture {
        state: Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
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
            oidc_rp,
            auth_pg,
            auth_db_url: db_url.to_string(),
            hydra_admin_url: "http://127.0.0.1:4445".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(zeroship_control::token_handlers::PatIssuer::dev_insecure()),
            hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
                "http://127.0.0.1:9",
            )),
            logout_jti_cache: Arc::new(
                zeroship_core::logout_token::LogoutJtiCache::default(),
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
        .send()
        .await
        .expect("unauthenticated control response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let pat = common::authz_fixture::admin_pat(&fixture.state).await;
    let response = control
        .get(format!("/api/apps/{app_id}/logs"))
        .header("authorization", pat.bearer())
        .send()
        .await
        .expect("control response");
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.body().await.expect("body");
    let lines: Vec<String> = serde_json::from_slice(&body).expect("logs json");
    eprintln!("[app_logs_http_test] captured logs: {lines:?}");
    assert_eq!(lines, vec![format!("b2-control-route-log {app_id}")]);
    pat.cleanup(&fixture.state).await;
}
