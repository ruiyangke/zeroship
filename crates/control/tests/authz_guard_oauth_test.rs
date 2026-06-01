//! Regression coverage for AuthzGuard's Hydra OAuth bearer branch.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use compio_postgres::{connect, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    api, authz_guard::AuthzGuard, token_handlers, AppState, EnvStore, Quota,
    RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_authz::{Action, Resource};
use zeroship_core::hydra::HydraIntrospector;

#[allow(dead_code)]
mod common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const OAUTH_TOKEN: &str = "fake-oauth-token";

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-authz-guard-oauth-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

#[derive(Debug, Clone)]
struct MockMode {
    status: u16,
    body: Value,
}

#[derive(Debug)]
struct MockState {
    mode: MockMode,
}

struct MockHydra {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockHydra {
    fn active(sub: impl ToString, scope: &str) -> Self {
        Self::active_with_aud(sub, scope, vec!["control.zeroship.ai"])
    }

    fn active_with_aud(sub: impl ToString, scope: &str, aud: Vec<&str>) -> Self {
        Self::fixed(
            200,
            json!({
                "active": true,
                "sub": sub.to_string(),
                "scope": scope,
                "aud": aud,
                "client_id": "oauth-test-client",
                "exp": unix_now_secs() + 3600,
            }),
        )
    }

    fn inactive() -> Self {
        Self::fixed(200, json!({ "active": false }))
    }

    fn fixed(status: u16, body: Value) -> Self {
        let state = Arc::new(MockState {
            mode: MockMode { status, body },
        });
        let factory_state = state.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("control-authz-oauth-mock")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = web::test::server(move || {
                        let state = factory_state.clone();
                        async move {
                            web::App::new().state(state).service(
                                web::resource("/admin/oauth2/introspect")
                                    .route(web::post().to(introspect_handler)),
                            )
                        }
                    })
                    .await;
                    let addr = server.addr();
                    started_tx.send(addr).expect("send mock server addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let addr = started_rx.recv().expect("mock server starts");
        Self {
            base: format!("http://{addr}"),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }
}

impl Drop for MockHydra {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, Deserialize)]
struct IntrospectForm {
    token: String,
}

async fn introspect_handler(
    state: web::types::State<Arc<MockState>>,
    form: web::types::Form<IntrospectForm>,
) -> HttpResponse {
    assert_eq!(form.token, OAUTH_TOKEN);
    let status = StatusCode::from_u16(state.mode.status).expect("valid status");
    HttpResponse::build(status).json(&state.mode.body)
}

struct Fixture {
    state: Arc<AppState>,
    user_id: Uuid,
    app_id: Option<Uuid>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Fixture {
    async fn cleanup(&self) {
        let _ = self
            .state
            .control_pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&self.user_id],
            )
            .await;
        if let Some(app_id) = self.app_id {
            let app_id_text = app_id.to_string();
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.app_members WHERE app_id = $1 OR user_id = $2",
                    &[&app_id_text, &self.user_id],
                )
                .await;
            let _ = self
                .state
                .control_pg
                .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
                .await;
        }
        let _ = self
            .state
            .control_pg
            .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&self.user_id])
            .await;
        let _ = self
            .state
            .control_pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id])
            .await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn fixture_with_hydra(hydra: &MockHydra, label: &str, user_id: Uuid) -> Option<Fixture> {
    let Some(db_url) = db_url() else {
        eprintln!("[authz_guard_oauth_test] AUTH_DB_URL not set - skipping");
        return None;
    };

    let (control_pg_client, control_pg_conn) = connect(&db_url, NoTls).await.expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
    let registry = Registry::new(&db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let vfs: Arc<dyn BundleStore + Send + Sync> =
        Arc::new(LocalFs::new(blob_root.join("legacy-bundles")).expect("vfs"));

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        vfs,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: false,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg: Arc::new(control_pg_client),
        hydra_admin_url: hydra.base.clone(),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(token_handlers::PatIssuer::dev_insecure()),
        hydra_introspector: Arc::new(HydraIntrospector::new(&hydra.base)),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        pairwise_salt: [0u8; 32],
    });

    insert_user(&state, user_id, label).await;
    Some(Fixture {
        state,
        user_id,
        app_id: None,
        blob_root,
        deploy_tmp_dir,
    })
}

async fn insert_user(state: &AppState, user_id: Uuid, label: &str) {
    let email = format!("{label}-{user_id}@zeroship.test");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW())",
            &[&user_id, &email, &label],
        )
        .await
        .expect("insert oauth test user");
}

async fn grant_platform_role(state: &AppState, user_id: Uuid, role: &str) {
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) VALUES ($1, $2, $1)",
            &[&user_id, &role],
        )
        .await
        .expect("insert platform role");
}

async fn create_app(fx: &mut Fixture, label: &str) -> Uuid {
    let app_name = format!("{label}-{}", Uuid::new_v4().simple());
    let record = fx
        .state
        .registry
        .create_app(&app_name, "free")
        .await
        .expect("create app");
    fx.app_id = Some(record.id);
    record.id
}

async fn grant_app_member(state: &AppState, app_id: Uuid, user_id: Uuid, role: &str) {
    let app_id = app_id.to_string();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, $3)",
            &[&app_id, &user_id, &role],
        )
        .await
        .expect("insert app member");
}

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(
            web::App::new()
                .state($fx.state.clone())
                .service(
                    web::resource("/api/apps")
                        .route(web::post().to(api::create_app))
                        .route(web::get().to(api::list_apps)),
                )
                .service(
                    web::resource("/api/apps/{id}")
                        .route(web::get().to(api::get_app))
                        .route(web::delete().to(api::delete_app)),
                )
                .service(
                    web::resource("/api/apps/{id}/deploy")
                        .route(web::post().to(api::deploy)),
                )
                .service(
                    web::resource("/raw-app/{id}")
                        .route(web::get().to(raw_app_read)),
                ),
        )
        .await
    }};
}

async fn raw_app_read(
    path: web::types::Path<String>,
    authz: AuthzGuard,
    state: web::types::State<Arc<AppState>>,
) -> web::HttpResponse {
    match authz
        .require(
            Action::AppsRead,
            Resource::App {
                id: path.into_inner(),
            },
            &state,
        )
        .await
    {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(resp) => resp,
    }
}

fn bearer() -> String {
    format!("Bearer {OAUTH_TOKEN}")
}

#[compio::test]
async fn oauth_token_with_apps_read_can_list_apps() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active(user_id, "apps:read apps:deploy");
    let Some(fx) = fixture_with_hydra(&hydra, "apps-read", user_id).await else {
        return;
    };
    let app = init_control!(fx);
    let request_id = format!("req_h4_{}", Uuid::new_v4().simple());

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer())
        .header("x-request-id", request_id.as_str())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT request_id \
             FROM zeroship.authz_decisions \
             WHERE actor_user_id = $1 AND action = 'apps:read' \
             ORDER BY occurred_at DESC \
             LIMIT 1",
            &[&user_id],
        )
        .await
        .expect("select authz decision");
    assert_eq!(
        rows.first()
            .map(|row| row.get::<_, Option<String>>("request_id"))
            .flatten()
            .as_deref(),
        Some(request_id.as_str())
    );

    fx.cleanup().await;
}

#[compio::test]
async fn oauth_token_without_required_scope_returns_403() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active(user_id, "apps:read");
    let Some(mut fx) = fixture_with_hydra(&hydra, "missing-deploy", user_id).await else {
        return;
    };
    grant_platform_role(&fx.state, user_id, "admin").await;
    let app_id = create_app(&mut fx, "missing-deploy").await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    fx.cleanup().await;
}

#[compio::test]
async fn inactive_oauth_token_returns_401() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::inactive();
    let Some(fx) = fixture_with_hydra(&hydra, "inactive", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("inactive token json");
    assert_eq!(body["error"], "inactive_token");

    fx.cleanup().await;
}

#[compio::test]
async fn oauth_token_wrong_audience_returns_401() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active_with_aud(user_id, "apps:read", vec!["gateway"]);
    let Some(fx) = fixture_with_hydra(&hydra, "wrong-audience", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    fx.cleanup().await;
}

#[compio::test]
async fn invalid_oauth_sub_returns_401() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active("not-a-uuid", "apps:read");
    let Some(fx) = fixture_with_hydra(&hydra, "invalid-sub", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    fx.cleanup().await;
}

#[compio::test]
async fn unknown_scope_returns_401_not_silently_dropped() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active(user_id, "apps:read bogus:scope");
    let Some(fx) = fixture_with_hydra(&hydra, "unknown-scope", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    fx.cleanup().await;
}

#[compio::test]
async fn invalid_app_resource_id_returns_400_before_cedar() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active(user_id, "apps:read");
    let Some(fx) = fixture_with_hydra(&hydra, "invalid-resource-id", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/raw-app/app%22%3B%20permit%20%28principal%2C%20action%2C%20resource%29%3B")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    fx.cleanup().await;
}

#[compio::test]
async fn oauth_token_subset_of_user_two_call_enforcement() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active(user_id, "apps:read");
    let Some(mut fx) = fixture_with_hydra(&hydra, "token-subset", user_id).await else {
        return;
    };
    grant_platform_role(&fx.state, user_id, "admin").await;
    let app_id = create_app(&mut fx, "token-subset").await;
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let req = test::TestRequest::delete()
        .uri(&format!("/api/apps/{app_id}"))
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    fx.cleanup().await;
}

#[compio::test]
async fn user_without_admin_role_oauth_scope_does_not_grant_apps_delete() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active(user_id, "apps:delete");
    let Some(mut fx) = fixture_with_hydra(&hydra, "user-subset", user_id).await else {
        return;
    };
    let app_id = create_app(&mut fx, "user-subset").await;
    grant_app_member(&fx.state, app_id, user_id, "viewer").await;
    let app = init_control!(fx);

    let req = test::TestRequest::delete()
        .uri(&format!("/api/apps/{app_id}"))
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    fx.cleanup().await;
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
