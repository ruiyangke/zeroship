//! Live-PG regression tests for user-facing OAuth grant handlers.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use chrono::{Duration, Utc};
use compio_postgres::{connect, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test, HttpRequest, HttpResponse};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_authz::{policy_hash, Action, Effect, Policy, Resource, Statement};
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    oauth_grants_handlers, oidc_rp, token_handlers, AppState, EnvStore, Quota, RateLimiter,
    Registry, SecretString, StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-oauth-grants-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    hydra: MockHydra,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Fixture {
    async fn new(db_url: &str, label: &str) -> Self {
        let hydra = MockHydra::start();
        let (auth_pg_client, auth_pg_conn) = connect(db_url, NoTls).await.expect("auth-pg connect");
        compio::runtime::spawn(async move {
            let _ = auth_pg_conn.run().await;
        })
        .detach();
        zeroship_auth::store::migrations::migrate(&auth_pg_client)
            .await
            .expect("auth migrations");

        let blob_root = tmpdir(&format!("blob-{label}"));
        let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
        let registry = Registry::new(db_url).await.expect("registry");
        let env_store =
            EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
        let stripe_store = StripeStore::new(registry.clone());
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let vfs: Arc<dyn BundleStore + Send + Sync> =
            Arc::new(LocalFs::new(blob_root.join("legacy-bundles")).expect("vfs"));
        let oidc_rp = Arc::new(oidc_rp::ConsoleOidcRp::new(
            "http://localhost:4444",
            "console.zeroship.ai",
            "test-oidc-secret".to_string(),
            b"test-stash-key".to_vec(),
        ));

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
            oidc_rp,
            auth_pg: Arc::new(auth_pg_client),
            auth_db_url: db_url.to_string(),
            hydra_admin_url: hydra.base.clone(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(token_handlers::PatIssuer::dev_insecure()),
            hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
                "http://127.0.0.1:9",
            )),
            logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        });

        Self {
            state,
            hydra,
            blob_root,
            deploy_tmp_dir,
        }
    }

    async fn cleanup_clients(&self, client_ids: &[String]) {
        if client_ids.is_empty() {
            return;
        }
        let ids = client_ids.iter().map(String::as_str).collect::<Vec<_>>();
        let _ = self
            .state
            .auth_pg
            .execute(
                "DELETE FROM control.oauth_grants WHERE client_id = ANY($1)",
                &[&ids],
            )
            .await;
        let _ = self
            .state
            .auth_pg
            .execute(
                "DELETE FROM control.oauth_clients WHERE client_id = ANY($1)",
                &[&ids],
            )
            .await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

#[derive(Clone, Debug)]
struct RecordedHydraRequest {
    method: String,
    path: String,
}

#[derive(Default)]
struct MockHydraState {
    requests: Vec<RecordedHydraRequest>,
}

struct MockHydra {
    base: String,
    state: Arc<Mutex<MockHydraState>>,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockHydra {
    fn start() -> Self {
        let state = Arc::new(Mutex::new(MockHydraState::default()));
        let factory_state = state.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("mock-hydra-oauth-grants")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = web::test::server(move || {
                        let state = factory_state.clone();
                        async move {
                            web::App::new().state(state).service(
                                web::resource("/admin/oauth2/auth/sessions/consent")
                                    .route(web::delete().to(mock_revoke_consent)),
                            )
                        }
                    })
                    .await;
                    let addr = server.addr();
                    started_tx.send(addr).expect("send mock hydra addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let addr = started_rx.recv().expect("mock hydra starts");
        Self {
            base: format!("http://{addr}"),
            state,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<RecordedHydraRequest> {
        self.state.lock().expect("mock hydra state").requests.clone()
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

async fn mock_revoke_consent(
    req: HttpRequest,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> HttpResponse {
    let path = if req.query_string().is_empty() {
        "/admin/oauth2/auth/sessions/consent".to_string()
    } else {
        format!(
            "/admin/oauth2/auth/sessions/consent?{}",
            req.query_string()
        )
    };
    state
        .lock()
        .expect("mock hydra state")
        .requests
        .push(RecordedHydraRequest {
            method: "DELETE".to_string(),
            path,
        });
    HttpResponse::NoContent().finish()
}

struct AccountPat {
    user_id: Uuid,
    token: String,
}

impl AccountPat {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    async fn cleanup(&self, state: &AppState) {
        cleanup_user(state, self.user_id).await;
    }
}

async fn account_pat(state: &AppState, label: &str) -> AccountPat {
    let user_id = insert_user(state, label).await;
    let token_id = Uuid::new_v4();
    let policies = account_policy().to_json_value();
    let hash = policy_hash(&policies);
    let expires_at = Utc::now() + Duration::days(1);
    let token = state
        .pat_issuer
        .issue(token_id, user_id, hash.clone(), expires_at)
        .expect("issue account PAT");

    state
        .auth_pg
        .execute(
            "INSERT INTO control.permission_tokens \
                (id, owner_id, kind, name, policies, policy_hash, expires_at) \
             VALUES ($1, $2, 'pat', 'integration account PAT', $3, $4, $5)",
            &[&token_id, &user_id, &policies, &hash, &expires_at],
        )
        .await
        .expect("insert account PAT row");

    AccountPat {
        user_id,
        token,
    }
}

fn account_policy() -> Policy {
    Policy {
        name: "account self-service".to_string(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::AccountRead, Action::AccountWrite],
            resources: vec![Resource::Any],
            conditions: Vec::new(),
        }],
    }
}

async fn insert_user(state: &AppState, label: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    let email = format!("{label}-{user_id}@zeroship.test");
    state
        .auth_pg
        .execute(
            "INSERT INTO auth.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW())",
            &[&user_id, &email, &label],
        )
        .await
        .expect("insert test user");
    user_id
}

async fn cleanup_user(state: &AppState, user_id: Uuid) {
    let _ = state
        .auth_pg
        .execute(
            "DELETE FROM control.authz_decisions WHERE user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = state
        .auth_pg
        .execute(
            "DELETE FROM control.permission_tokens WHERE owner_id = $1",
            &[&user_id],
        )
        .await;
    let _ = state
        .auth_pg
        .execute(
            "DELETE FROM control.oauth_grants WHERE user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = state
        .auth_pg
        .execute("DELETE FROM platform.roles WHERE user_id = $1", &[&user_id])
        .await;
    let _ = state
        .auth_pg
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user_id])
        .await;
}

async fn insert_client(state: &AppState, client_id: &str, created_by: Uuid) {
    let redirect_uri = format!("https://{client_id}.example/callback");
    let redirect_uris = vec![redirect_uri.as_str()];
    let scopes = vec!["apps:read", "env:read"];
    state
        .auth_pg
        .execute(
            "INSERT INTO control.oauth_clients \
                (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                 skip_consent, created_by, hydra_client_id) \
             VALUES ($1, $2, $3, $4, $5, $6, false, $7, $1)",
            &[
                &client_id,
                &format!("Client {client_id}"),
                &Some(format!("https://{client_id}.example")),
                &Some(format!("https://{client_id}.example/logo.png")),
                &redirect_uris,
                &scopes,
                &created_by,
            ],
        )
        .await
        .expect("insert oauth client");
}

async fn insert_grant(state: &AppState, user_id: Uuid, client_id: &str, scopes: &[&str]) {
    let granted_scopes = scopes.to_vec();
    state
        .auth_pg
        .execute(
            "INSERT INTO control.oauth_grants \
                (user_id, client_id, granted_scopes, granted_at, last_used_at) \
             VALUES ($1, $2, $3, NOW(), NOW())",
            &[&user_id, &client_id, &granted_scopes],
        )
        .await
        .expect("insert oauth grant");
}

async fn count_grant(state: &AppState, user_id: Uuid, client_id: &str) -> i64 {
    let rows = state
        .auth_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM control.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&user_id, &client_id],
        )
        .await
        .expect("count oauth grant");
    rows[0].get("n")
}

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(
            web::App::new()
                .state($fx.state.clone())
                .configure(oauth_grants_handlers::configure),
        )
        .await
    }};
}

#[compio::test]
async fn list_returns_empty_when_no_grants() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "empty").await;
    let pat = account_pat(&fx.state, "empty").await;
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/me/oauth-grants")
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    assert_eq!(body, json!([]));

    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn list_returns_user_grants_with_client_metadata() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "metadata").await;
    let pat = account_pat(&fx.state, "metadata").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-metadata-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, pat.user_id).await;
    insert_grant(&fx.state, pat.user_id, &client_id, &["apps:read", "env:read"]).await;

    let req = test::TestRequest::get()
        .uri("/me/oauth-grants")
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    let grants = body.as_array().expect("grants array");
    assert_eq!(grants.len(), 1);
    let grant = &grants[0];
    assert_eq!(grant["client_id"].as_str(), Some(client_id.as_str()));
    assert_eq!(grant["client_name"], format!("Client {client_id}"));
    assert_eq!(
        grant["client_uri"],
        format!("https://{client_id}.example")
    );
    assert_eq!(
        grant["logo_uri"],
        format!("https://{client_id}.example/logo.png")
    );
    assert_eq!(grant["granted_scopes"], json!(["apps:read", "env:read"]));
    assert!(grant["granted_at"].as_str().is_some_and(|value| value.contains('T')));
    assert!(grant["last_used_at"].as_str().is_some_and(|value| value.contains('T')));

    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn list_does_not_leak_other_users_grants() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "isolation").await;
    let pat = account_pat(&fx.state, "isolation-a").await;
    let other_user = insert_user(&fx.state, "isolation-b").await;
    let app = init_control!(fx);
    let client_a = format!("oauth-grant-a-{}", Uuid::new_v4().simple());
    let client_b = format!("oauth-grant-b-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_a, pat.user_id).await;
    insert_client(&fx.state, &client_b, pat.user_id).await;
    insert_grant(&fx.state, pat.user_id, &client_a, &["apps:read"]).await;
    insert_grant(&fx.state, other_user, &client_b, &["env:read"]).await;

    let req = test::TestRequest::get()
        .uri("/me/oauth-grants")
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    let grants = body.as_array().expect("grants array");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["client_id"].as_str(), Some(client_a.as_str()));

    fx.cleanup_clients(&[client_a, client_b]).await;
    cleanup_user(&fx.state, other_user).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn revoke_removes_grant_row() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "revoke-row").await;
    let pat = account_pat(&fx.state, "revoke-row").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-revoke-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, pat.user_id).await;
    insert_grant(&fx.state, pat.user_id, &client_id, &["apps:read"]).await;

    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(count_grant(&fx.state, pat.user_id, &client_id).await, 0);

    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn revoke_revokes_hydra_tokens_for_user_client_pair() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "revoke-hydra").await;
    let pat = account_pat(&fx.state, "revoke-hydra").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-hydra-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, pat.user_id).await;
    insert_grant(&fx.state, pat.user_id, &client_id, &["apps:read"]).await;

    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let requests = fx.hydra.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "DELETE");
    assert!(
        requests[0]
            .path
            .starts_with("/admin/oauth2/auth/sessions/consent?")
    );
    assert!(requests[0].path.contains(&format!("subject={}", pat.user_id)));
    assert!(requests[0].path.contains(&format!("client={client_id}")));

    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn revoke_returns_404_when_no_grant() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "missing").await;
    let pat = account_pat(&fx.state, "missing").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-missing-{}", Uuid::new_v4().simple());

    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(fx.hydra.requests().is_empty());

    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn revoke_does_not_affect_other_users() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "other-user").await;
    let owner = account_pat(&fx.state, "other-user-owner").await;
    let revoker = account_pat(&fx.state, "other-user-revoker").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-other-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, owner.user_id).await;
    insert_grant(&fx.state, owner.user_id, &client_id, &["apps:read"]).await;

    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", revoker.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(count_grant(&fx.state, owner.user_id, &client_id).await, 1);
    assert!(fx.hydra.requests().is_empty());

    fx.cleanup_clients(&[client_id]).await;
    revoker.cleanup(&fx.state).await;
    owner.cleanup(&fx.state).await;
}

#[compio::test]
async fn unauthenticated_request_returns_401() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "unauth").await;
    let app = init_control!(fx);

    let req = test::TestRequest::get().uri("/me/oauth-grants").to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
