//! Regression coverage for AuthzGuard's Hydra OAuth bearer branch.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::{connect, NoTls};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::http::StatusCode;
use ntex::web::{self, test, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    api, authz_guard::AuthzGuard, token_handlers, AppState, EnvStore, Quota,
    RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_authz::{Action, Resource};
use zeroship_core::auth_provider::{
    AuthProvider, DualIssuerProvider, HydraProvider, LegacyAuthProvider, PlatformConfig,
    PlatformProvider,
};

#[allow(dead_code)]
mod common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const LEGACY_HYDRA_ISSUER: &str = "https://hydra.zeroship.test";
const PLATFORM_ISSUER: &str = "https://auth.zeroship.test";
const PLATFORM_KID: &str = "platform-control-authz-kid";
const PLATFORM_KEY_SEED: u8 = 31;
const OAUTH_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJpc3MiOiJodHRwczovL2h5ZHJhLnplcm9zaGlwLnRlc3QifQ.";

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
    fixture_with_auth_provider(
        hydra,
        label,
        user_id,
        zeroship_control::hydra_auth_provider(&hydra.base),
    )
    .await
}

async fn fixture_with_auth_provider(
    hydra: &MockHydra,
    label: &str,
    user_id: Uuid,
    auth_provider: Arc<AuthProvider>,
) -> Option<Fixture> {
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
    zeroship_control::bootstrap_console::seed_plans(&registry).await.expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new(String::new()),
        stripe_base_url: "https://api.stripe.com".to_string(),
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
        auth_provider,
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: std::sync::Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
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

/// Create an app owned by the fixture's principal (binds the owner membership).
async fn create_app(fx: &mut Fixture, label: &str) -> Uuid {
    let owner = fx.user_id;
    create_app_owned_by(fx, label, owner).await
}

/// Create an app owned by an arbitrary `owner_id` (which may differ from the
/// fixture principal). Used to exercise the "principal is only a viewer of an
/// app someone else owns" case — the create now binds an owner membership, so
/// tests that need the principal to NOT be the owner must seed a distinct owner.
async fn create_app_owned_by(fx: &mut Fixture, label: &str, owner_id: Uuid) -> Uuid {
    if owner_id != fx.user_id {
        // The owner must exist (FK on app_members.user_id → users.id).
        insert_user(&fx.state, owner_id, &format!("{label}-owner")).await;
    }
    let app_name = format!("{label}-{}", Uuid::new_v4().simple());
    let record = fx
        .state
        .registry
        .create_app(&app_name, &zeroship_control::bootstrap_console::free_plan_id(), &owner_id)
        .await
        .expect("create app");
    fx.app_id = Some(record.id);
    record.id
}

async fn grant_app_member(state: &AppState, app_id: Uuid, user_id: Uuid, role: &str) {
    // app_members.app_id is a uuid column — bind the Uuid directly (binding a
    // String panics with WrongType against the uuid column).
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
                )
                .service(
                    web::resource("/raw-app/{id}/deploy-check")
                        .route(web::post().to(raw_app_deploy_check)),
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

async fn raw_app_deploy_check(
    path: web::types::Path<String>,
    authz: AuthzGuard,
    state: web::types::State<Arc<AppState>>,
) -> web::HttpResponse {
    match authz
        .require(
            Action::AppsDeploy,
            Resource::App {
                id: path.into_inner(),
            },
            &state,
        )
        .await
    {
        Ok(()) => web::HttpResponse::Ok().json(&json!({
            "principal_id": authz.principal_id.to_string(),
        })),
        Err(resp) => resp,
    }
}

fn bearer() -> String {
    format!("Bearer {OAUTH_TOKEN}")
}

fn bearer_for(token: &str) -> String {
    format!("Bearer {token}")
}

fn platform_auth_provider(jwks_url: String, hydra_admin_url: &str) -> Arc<AuthProvider> {
    let platform = PlatformProvider::new(
        PlatformConfig::new(PLATFORM_ISSUER, Some(jwks_url)).expect("platform config"),
    );
    let legacy = LegacyAuthProvider::Hydra(HydraProvider::new_with_issuer(
        zeroship_core::hydra::HydraIntrospector::new(hydra_admin_url),
        LEGACY_HYDRA_ISSUER,
    ));
    Arc::new(AuthProvider::DualIssuer(DualIssuerProvider::new(
        platform,
        legacy,
    )))
}

fn platform_token(subject: Uuid, scope: &str, issuer: &str) -> String {
    let now = unix_now_secs();
    let claims = json!({
        "iss": issuer,
        "sub": subject.to_string(),
        "aud": "control.zeroship.ai",
        "exp": now + 3600,
        "iat": now,
        "nbf": now.saturating_sub(1),
        "jti": Uuid::new_v4().to_string(),
        "client_id": "zeroship-cli",
        "scope": scope,
    });
    let mut header = Header::new(Algorithm::EdDSA);
    header.typ = Some("at+jwt".to_string());
    header.kid = Some(PLATFORM_KID.to_string());
    encode(&header, &claims, &platform_encoding_key()).expect("platform token")
}

fn platform_encoding_key() -> EncodingKey {
    let sk = SigningKey::from_bytes(&[PLATFORM_KEY_SEED; 32]);
    let pkcs8 = sk.to_pkcs8_der().expect("encode pkcs8");
    EncodingKey::from_ed_der(pkcs8.as_bytes())
}

fn platform_jwks_body() -> String {
    let sk = SigningKey::from_bytes(&[PLATFORM_KEY_SEED; 32]);
    json!({
        "keys": [{
            "kid": PLATFORM_KID,
            "kty": "OKP",
            "alg": "EdDSA",
            "crv": "Ed25519",
            "x": URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes()),
        }]
    })
    .to_string()
}

struct PlatformJwksMock {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PlatformJwksMock {
    fn start() -> Self {
        let body = Arc::new(RwLock::new(platform_jwks_body()));
        let factory_body = body.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("control-platform-jwks-mock")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = web::test::server(move || {
                        let body = factory_body.clone();
                        async move {
                            web::App::new().state(body).service(
                                web::resource("/.well-known/jwks.json")
                                    .route(web::get().to(platform_jwks_handler)),
                            )
                        }
                    })
                    .await;
                    let addr = server.addr();
                    started_tx.send(addr).expect("send platform jwks addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let addr = started_rx.recv().expect("platform jwks mock starts");
        Self {
            base: format!("http://{addr}"),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.base)
    }
}

impl Drop for PlatformJwksMock {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn platform_jwks_handler(body: web::types::State<Arc<RwLock<String>>>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(body.read().expect("jwks body lock").clone())
}

#[compio::test]
async fn dual_issuer_accepts_platform_deploy_and_legacy_hydra_but_rejects_unknown_issuer() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active(user_id, "apps:read");
    let jwks = PlatformJwksMock::start();
    let auth_provider = platform_auth_provider(jwks.jwks_url(), &hydra.base);
    let Some(mut fx) =
        fixture_with_auth_provider(&hydra, "dual-issuer", user_id, auth_provider).await
    else {
        return;
    };
    let app_id = create_app(&mut fx, "dual-issuer").await;
    let app = init_control!(fx);

    let platform_deploy = platform_token(user_id, "apps:deploy", PLATFORM_ISSUER);
    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{app_id}/deploy-check"))
        .header("authorization", bearer_for(&platform_deploy))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("platform body json");
    assert_eq!(body["principal_id"], user_id.to_string());

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "legacy Hydra-shaped token must still verify during issuer migration"
    );

    let unknown_issuer = platform_token(user_id, "apps:read", "https://unknown-issuer.test");
    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer_for(&unknown_issuer))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    fx.cleanup().await;
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

/// F3 regression — drives the REAL self-service path end to end with NO
/// pre-seeded `app_members` / platform role:
///
///   1. A default-role creator (OAuth token, scopes `apps:write apps:read`)
///      POSTs `/api/apps` → 201. This exercises the broadened create gate AND
///      `registry::create_app` binding the principal as the app's `owner` row.
///   2. The same creator GETs `/api/apps` → 200 and sees EXACTLY the app they
///      just created (ownership-scoped list).
///   3. Another creator's app (owned by a different principal) is NOT visible.
///
/// Before the fix step 1 was a 403 (no policy granted a default-role creator
/// `apps:write` on `Resource::Any`) and there was no owner-binding at all, so a
/// creator was locked out of their own apps. This test must NOT pre-seed any
/// `app_members` row for the principal — the owner row has to come from the
/// production create path.
#[compio::test]
async fn creator_self_service_creates_and_lists_only_own_apps() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active(user_id, "apps:write apps:read");
    let Some(mut fx) = fixture_with_hydra(&hydra, "self-service", user_id).await else {
        return;
    };

    // Seed ANOTHER creator's app (different owner) directly. It must never show
    // up in this principal's scoped list.
    let other_owner = Uuid::new_v4();
    insert_user(&fx.state, other_owner, "self-service-other").await;
    let other_app = fx
        .state
        .registry
        .create_app(
            &format!("otherapp-{}", Uuid::new_v4().simple()),
            &zeroship_control::bootstrap_console::free_plan_id(),
            &other_owner,
        )
        .await
        .expect("create other-owner app");

    let app = init_control!(fx);

    // 1. Create an app as the default-role creator via the production handler.
    let create_name = format!("mine-{}", Uuid::new_v4().simple());
    let req = test::TestRequest::post()
        .uri("/api/apps")
        .header("authorization", bearer())
        .set_json(&json!({ "name": create_name }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "default-role creator must be able to create their own app"
    );
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("create body json");
    let created_id = body["id"].as_str().expect("created app id").to_string();
    // Track for fixture cleanup (FK cascade removes the owner membership).
    fx.app_id = Some(Uuid::parse_str(&created_id).expect("uuid"));

    // 2. List — the creator sees their app, scoped to ownership.
    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("list body json");
    let ids: Vec<String> = list
        .as_array()
        .expect("list is array")
        .iter()
        .map(|app| app["id"].as_str().expect("app id").to_string())
        .collect();
    assert!(
        ids.contains(&created_id),
        "creator must see their OWN app in the scoped list (got {ids:?})"
    );
    // 3. The other creator's app must NOT leak into this principal's list.
    assert!(
        !ids.contains(&other_app.id.to_string()),
        "scoped list must NOT include another tenant's app"
    );

    // Cleanup the other-owner app + user (fixture cleanup handles `created_id`).
    let _ = fx
        .state
        .control_pg
        .execute(
            "DELETE FROM zeroship.app_members WHERE app_id = $1",
            &[&other_app.id],
        )
        .await;
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&other_app.id])
        .await;
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&other_owner])
        .await;
    fx.cleanup().await;
}

/// 7.0 regression — a platform `billing` staffer holds fleet-wide `apps:read`
/// authority (billing.cedar grants `apps:read` on an unconstrained resource, so
/// `get_app` lets billing read ANY single app by id), but the `/api/apps` LIST
/// endpoint must be consistent with that authority and return the whole fleet,
/// not just apps the staffer happens to own.
///
/// Before the fix `list_apps`'s `fleet_wide_reader` SQL was
/// `role IN ('admin','readonly','support')` — `billing` omitted — so a
/// member-less billing staffer fell through to `list_apps_for_owner` and got an
/// EMPTY list for an app it is authorized to (and can, via `get_app`) read.
///
/// This test seeds NO `app_members` row for the billing principal and a foreign
/// app owned by someone else; the staffer must still see that foreign app in the
/// list. It does NOT widen creator/default access: the principal here holds the
/// platform `billing` role, and the C1 cross-tenant deny for un-roled creators
/// is covered by `creator_self_service_creates_and_lists_only_own_apps`.
#[compio::test]
async fn billing_platform_role_lists_apps_fleet_wide() {
    let user_id = Uuid::new_v4();
    let hydra = MockHydra::active(user_id, "apps:read");
    let Some(fx) = fixture_with_hydra(&hydra, "billing-fleet", user_id).await else {
        return;
    };
    // The staffer holds the platform `billing` role — fleet-wide `apps:read`
    // authority per billing.cedar — but NO membership on any app.
    grant_platform_role(&fx.state, user_id, "billing").await;

    // A foreign app owned by a different principal. The billing staffer is not a
    // member of it, yet is authorized to read it.
    let other_owner = Uuid::new_v4();
    insert_user(&fx.state, other_owner, "billing-fleet-other").await;
    let other_app = fx
        .state
        .registry
        .create_app(
            &format!("otherapp-{}", Uuid::new_v4().simple()),
            &zeroship_control::bootstrap_console::free_plan_id(),
            &other_owner,
        )
        .await
        .expect("create other-owner app");

    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("list body json");
    let ids: Vec<String> = list
        .as_array()
        .expect("list is array")
        .iter()
        .map(|app| app["id"].as_str().expect("app id").to_string())
        .collect();
    assert!(
        ids.contains(&other_app.id.to_string()),
        "billing staffer with fleet-wide apps:read authority must see the \
         foreign app in the list (got {ids:?})"
    );

    // Cleanup the foreign app + owner (fixture cleanup handles the principal).
    let _ = fx
        .state
        .control_pg
        .execute(
            "DELETE FROM zeroship.app_members WHERE app_id = $1",
            &[&other_app.id],
        )
        .await;
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&other_app.id])
        .await;
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&other_owner])
        .await;
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
    // The app is owned by a DIFFERENT principal; `user_id` is only a viewer.
    // (create_app now binds the creator as owner, so the principal-under-test
    // must NOT be the creator for this "viewer-only" scenario.)
    let owner_id = Uuid::new_v4();
    let app_id = create_app_owned_by(&mut fx, "user-subset", owner_id).await;
    grant_app_member(&fx.state, app_id, user_id, "viewer").await;
    let app = init_control!(fx);

    let req = test::TestRequest::delete()
        .uri(&format!("/api/apps/{app_id}"))
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    fx.cleanup().await;
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&owner_id])
        .await;
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
