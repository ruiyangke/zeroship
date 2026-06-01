//! HTTP regression tests for creator-console PAT handlers.
//!
//! Under the R5 cutover the bespoke console-session principal path is gone:
//! the control plane authenticates every request through the `AuthzGuard`
//! bearer path. PAT minting still requires an INTERACTIVE (non-PAT) principal,
//! which is now an OAuth/BFF session access token (the `oauth_guard_from_bearer`
//! arm: `token_id == None`). So these tests drive the handlers with an OAuth
//! Bearer introspected against a mock hydra (the SAME harness shape
//! `authz_guard_oauth_test` uses) rather than a console-session cookie.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use ntex::http::StatusCode;
use ntex::web::{self, test, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};

mod common;

const OAUTH_TOKEN: &str = "fake-console-session-token";

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

fn unix_now_secs() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs(),
    )
    .expect("clock fits i64")
}

// ── mock hydra introspection server (mirrors authz_guard_oauth_test) ────────
//
// The OAuth-bearer arm of `AuthzGuard` introspects the token against
// hydra-admin. The mock returns the fixture user as `sub` with a broad scope —
// the OAuth scope does NOT gate PAT minting (the grant-subset check uses the
// principal's DB-side platform role / app membership, with `token_policy =
// None`), it only needs to resolve to a valid non-PAT principal.
#[derive(Debug)]
struct MockState {
    body: Value,
}

struct MockHydra {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockHydra {
    fn active(sub: impl ToString) -> Self {
        // Scope must be the authz platform vocabulary only (`parse_scope_string`
        // rejects bare OIDC identity scopes like `openid`). The OAuth scope does
        // NOT gate PAT minting — `validate_grant_subset` checks the principal's
        // DB-side platform role with `token_policy = None` — so any valid
        // platform scope suffices to resolve the non-PAT principal.
        let body = json!({
            "active": true,
            "sub": sub.to_string(),
            "scope": "apps:read apps:deploy",
            "aud": ["control.zeroship.ai"],
            "client_id": "oac_console_test",
            "exp": unix_now_secs() + 3600,
        });
        let state = Arc::new(MockState { body });
        let factory_state = state.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("control-token-handlers-oauth-mock")
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
    HttpResponse::Ok().json(&state.body)
}

fn bearer() -> String {
    format!("Bearer {OAUTH_TOKEN}")
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-token-handlers-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    user_id: Uuid,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    // Keep the mock introspection server alive for the test's lifetime.
    _hydra: MockHydra,
}

impl Fixture {
    async fn new(db_url: &str, label: &str, platform_role: Option<&str>) -> Self {
        let blob_root = tmpdir(&format!("blob-{label}"));
        let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));

        let registry = Registry::new(db_url).await.expect("registry");

        let env_store = EnvStore::new(registry.clone(), "test-master-key", false)
            .expect("env store");
        let stripe_store = StripeStore::new(registry.clone());
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let vfs: Arc<dyn BundleStore + Send + Sync> = Arc::new(
            LocalFs::new(blob_root.join("legacy-bundles")).expect("vfs"),
        );

        let (auth_pg_client, auth_pg_conn) =
            compio_postgres::connect(db_url, compio_postgres::NoTls)
                .await
                .expect("auth-pg connect");
        compio::runtime::spawn(async move {
            let _ = auth_pg_conn.run().await;
        })
        .detach();
        let auth_pg = Arc::new(auth_pg_client);

        // The acting creator. Created BEFORE the mock introspector so its `sub`
        // resolves to this user; its platform role drives the grant ceiling.
        let user_id = Uuid::new_v4();
        let email = format!("{label}-{user_id}@zeroship.test");
        auth_pg
            .execute(
                "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
                &[&user_id, &email, &label],
            )
            .await
            .expect("insert user");
        if let Some(role) = platform_role {
            auth_pg
                .execute(
                    "INSERT INTO zeroship.platform_admin_roles (user_id, role) VALUES ($1, $2)",
                    &[&user_id, &role],
                )
                .await
                .expect("insert platform role");
        }

        let hydra = MockHydra::active(user_id);

        let state = Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            vfs,
            blob_store,
            control_key: SecretString::new("test-control-key".to_string()),
            master_key: SecretString::new("test-master-key".to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            insecure_dev: false,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            auth_pg,
            auth_db_url: db_url.to_string(),
            hydra_admin_url: hydra.base.clone(),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(token_handlers::PatIssuer::dev_insecure()),
            hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
                &hydra.base,
            )),
            logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
            pairwise_salt: [0u8; 32],
        });

        Self {
            state,
            user_id,
            blob_root,
            deploy_tmp_dir,
            _hydra: hydra,
        }
    }

    async fn cleanup(&self) {
        let token_rows = self
            .state
            .auth_pg
            .query(
                "SELECT id FROM zeroship.permission_tokens WHERE owner_id = $1",
                &[&self.user_id],
            )
            .await
            .unwrap_or_default();
        for row in token_rows {
            let id: Uuid = row.get("id");
            let _ = self
                .state
                .auth_pg
                .execute("DELETE FROM zeroship.authz_decisions WHERE token_id = $1", &[&id])
                .await;
        }
        let _ = self
            .state
            .auth_pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = self
            .state
            .auth_pg
            .execute(
                "DELETE FROM zeroship.permission_tokens WHERE owner_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = self
            .state
            .auth_pg
            .execute(
                "DELETE FROM zeroship.app_members WHERE user_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = self
            .state
            .auth_pg
            .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&self.user_id])
            .await;
        let _ = self
            .state
            .auth_pg
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

fn deploy_policy() -> Value {
    json!({
        "name": "CI deploy",
        "statements": [{
            "effect": "allow",
            "actions": ["apps:deploy"],
            "resources": [{"type": "any"}]
        }]
    })
}

async fn audit_event_count(state: &AppState, user_id: Uuid, event_type: &str) -> i64 {
    let rows = state
        .auth_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM zeroship.audit_events \
             WHERE actor_user_id = $1 AND event_type = $2",
            &[&user_id, &event_type],
        )
        .await
        .expect("count audit events");
    rows[0].get("n")
}

macro_rules! create_pat {
    ($app:expr, $name:expr) => {{
        let body = json!({
            "name": $name,
            "policies": deploy_policy(),
            "expires_in_days": 90
        });
        let req = test::TestRequest::post()
            .uri("/me/tokens")
            .header("accept", "application/json")
            .header("authorization", bearer())
            .set_json(&body)
            .to_request();
        let resp = test::call_service(&$app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = test::read_body(resp).await;
        serde_json::from_slice::<Value>(&bytes).expect("create PAT JSON")
    }};
}

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(
            web::App::new()
                .state($fx.state.clone())
                .configure(token_handlers::configure),
        )
        .await
    }};
}

#[compio::test]
async fn create_pat_with_valid_policy_returns_jwt() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "valid", Some("admin")).await;
    let app = init_control!(fx);

    let json = create_pat!(app, "CI deploy");
    let id = json.get("id").and_then(Value::as_str).expect("id");
    let token = json
        .get("token")
        .and_then(Value::as_str)
        .expect("token");
    let claims = fx.state.pat_issuer.verify(token).expect("verify PAT");

    assert_eq!(claims.jti, id);
    assert_eq!(claims.owner, fx.user_id.to_string());
    assert_eq!(claims.aud, "control.zeroship.ai");
    let exp = claims.exp;
    let now = chrono::Utc::now().timestamp();
    assert!(exp > now + 89 * 86_400, "exp should be about 90 days out");
    assert!(exp <= now + 91 * 86_400, "exp should be about 90 days out");
    assert_eq!(
        audit_event_count(&fx.state, fx.user_id, "pat_mint").await,
        1
    );

    fx.cleanup().await;
}

#[compio::test]
async fn create_pat_with_policy_exceeding_user_returns_400() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "viewer", Some("readonly")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", bearer())
        .set_json(&json!({
            "name": "too broad",
            "policies": deploy_policy(),
            "expires_in_days": 90
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = test::read_body(resp).await;
    let body: Value = serde_json::from_slice(&bytes).expect("error body");
    assert_eq!(body.get("error").and_then(Value::as_str), Some("excess_permissions"));

    fx.cleanup().await;
}

#[compio::test]
async fn app_owner_can_create_any_resource_pat_for_owned_action() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "owner-any", None).await;
    let app_id = format!("app-{}", Uuid::new_v4().simple());
    fx.state
        .auth_pg
        .execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&app_id, &fx.user_id],
        )
        .await
        .expect("insert owner app member");
    let app = init_control!(fx);

    let created = create_pat!(app, "owner deploy");
    assert!(created.get("token").and_then(Value::as_str).is_some());

    fx.cleanup().await;
}

#[compio::test]
async fn create_pat_with_invalid_resource_id_returns_400() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "invalid-resource", Some("admin")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", bearer())
        .set_json(&json!({
            "name": "bad resource",
            "policies": {
                "name": "bad resource",
                "statements": [{
                    "effect": "allow",
                    "actions": ["apps:read"],
                    "resources": [{
                        "type": "app",
                        "id": "app\"; permit (principal, action, resource);"
                    }]
                }]
            },
            "expires_in_days": 90
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = test::read_body(resp).await;
    let body: Value = serde_json::from_slice(&bytes).expect("error body");
    assert_eq!(
        body.get("error").and_then(Value::as_str),
        Some("invalid_resource_id")
    );

    fx.cleanup().await;
}

#[compio::test]
async fn create_pat_with_empty_statement_actions_returns_400() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "empty-actions", Some("admin")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", bearer())
        .set_json(&json!({
            "name": "empty actions",
            "policies": {
                "name": "empty actions",
                "statements": [{
                    "effect": "allow",
                    "actions": [],
                    "resources": [{"type": "any"}]
                }]
            },
            "expires_in_days": 90
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = test::read_body(resp).await;
    let body: Value = serde_json::from_slice(&bytes).expect("error body");
    assert_eq!(
        body.get("error").and_then(Value::as_str),
        Some("empty_policy_statement")
    );

    fx.cleanup().await;
}

#[compio::test]
async fn create_pat_with_empty_statement_resources_returns_400() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "empty-resources", Some("admin")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", bearer())
        .set_json(&json!({
            "name": "empty resources",
            "policies": {
                "name": "empty resources",
                "statements": [{
                    "effect": "allow",
                    "actions": ["apps:read"],
                    "resources": []
                }]
            },
            "expires_in_days": 90
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = test::read_body(resp).await;
    let body: Value = serde_json::from_slice(&bytes).expect("error body");
    assert_eq!(
        body.get("error").and_then(Value::as_str),
        Some("empty_policy_statement")
    );

    fx.cleanup().await;
}

#[compio::test]
async fn create_pat_with_mfa_condition_returns_400() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "mfa-condition", Some("admin")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", bearer())
        .set_json(&json!({
            "name": "mfa condition",
            "policies": {
                "name": "mfa condition",
                "statements": [{
                    "effect": "allow",
                    "actions": ["apps:read"],
                    "resources": [{"type": "any"}],
                    "conditions": [{"kind": "require_mfa"}]
                }]
            },
            "expires_in_days": 90
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = test::read_body(resp).await;
    let body: Value = serde_json::from_slice(&bytes).expect("error body");
    assert_eq!(
        body.get("error").and_then(Value::as_str),
        Some("unsupported_policy_condition")
    );

    fx.cleanup().await;
}

#[compio::test]
async fn list_pats_returns_user_tokens_without_secret() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "list", Some("admin")).await;
    let app = init_control!(fx);

    create_pat!(app, "CI deploy A");
    create_pat!(app, "CI deploy B");

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = test::read_body(resp).await;
    let body: Value = serde_json::from_slice(&bytes).expect("list JSON");
    let entries = body.as_array().expect("array");
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry.get("token").is_none()));
    assert!(entries.iter().all(|entry| entry.get("name").is_some()));

    fx.cleanup().await;
}

#[compio::test]
async fn delete_pat_marks_revoked() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "delete", Some("admin")).await;
    let app = init_control!(fx);

    let created = create_pat!(app, "CI deploy");
    let id = created.get("id").and_then(Value::as_str).expect("id");
    let req = test::TestRequest::delete()
        .uri(&format!("/me/tokens/{id}"))
        .header("accept", "application/json")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = test::read_body(resp).await;
    let deleted: Value = serde_json::from_slice(&bytes).expect("delete JSON");
    assert!(deleted.get("revoked_at").and_then(Value::as_str).is_some());

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    let bytes = test::read_body(resp).await;
    let body: Value = serde_json::from_slice(&bytes).expect("list JSON");
    let entry = body
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
        .expect("revoked token listed");
    assert!(entry.get("revoked_at").and_then(Value::as_str).is_some());
    assert_eq!(
        audit_event_count(&fx.state, fx.user_id, "pat_revoke").await,
        1
    );

    fx.cleanup().await;
}

#[compio::test]
async fn using_revoked_pat_returns_401() {
    let Some(db_url) = db_url() else {
        eprintln!("[token_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "revoked-use", Some("admin")).await;
    let app = init_control!(fx);
    let pat = common::authz_fixture::admin_pat(&fx.state).await;

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    fx.state
        .auth_pg
        .execute(
            "UPDATE zeroship.permission_tokens SET revoked_at = NOW() WHERE id = $1",
            &[&pat.token_id],
        )
        .await
        .expect("revoke PAT");

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    pat.cleanup(&fx.state).await;
    fx.cleanup().await;
}
