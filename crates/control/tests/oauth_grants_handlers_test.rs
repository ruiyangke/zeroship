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
    api, oauth_grants_handlers, oidc_rp, token_handlers, AppState, EnvStore, Quota, RateLimiter,
    Registry, SecretString, StripeStore,
};

mod common;

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
        Self::new_with_auth_db_url(db_url, db_url, label).await
    }

    /// Build a fixture whose `registry` + `auth_pg` use the real `db_url` but
    /// whose `auth_db_url` (the URL the relay cascade opens a DEDICATED client
    /// on) is `auth_db_url`. Pointing `auth_db_url` at an unreachable address
    /// lets a test exercise the cascade's FAILURE arm in isolation — only the
    /// dedicated relay connection fails, while authz/registry/`auth_pg` keep
    /// using the real DB and succeed. This is exactly the seam the dedicated
    /// (non-shared) connection design restores.
    async fn new_with_auth_db_url(db_url: &str, auth_db_url: &str, label: &str) -> Self {
        let hydra = MockHydra::start();
        let (auth_pg_client, auth_pg_conn) = connect(db_url, NoTls).await.expect("auth-pg connect");
        compio::runtime::spawn(async move {
            let _ = auth_pg_conn.run().await;
        })
        .detach();

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
            auth_db_url: auth_db_url.to_string(),
            hydra_admin_url: hydra.base.clone(),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
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

/// Seed an `auth.app_user_identities` row with a minted relay alias keyed on
/// `(client_id, user_id)` — the row the gateway writes (Slice 4) + the alias
/// consent mints (5b). The relay revocation cascade (5c §6) revokes THIS row.
async fn insert_identity_with_alias(
    state: &AppState,
    client_id: &str,
    user_id: Uuid,
    relay_email: &str,
) {
    let pairwise_sub = format!("pws_test_{}", Uuid::new_v4().simple());
    state
        .auth_pg
        .execute(
            "INSERT INTO auth.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub, relay_email) \
             VALUES ($1, $2, $3, $4)",
            &[&client_id, &user_id, &pairwise_sub, &relay_email],
        )
        .await
        .expect("insert app_user_identities row");
}

/// The active-alias resolution the 5b relay webhook runs on EVERY inbound
/// (`relay::resolve_active_alias`, sub-spec §4.5). `None` ⇒ the webhook emits a
/// bounce + 200 (the revoked/unknown-alias branch). We assert against the REAL
/// auth-store gate, not a stub, so this is the faithful cross-service seam.
async fn alias_is_active(state: &AppState, relay_email: &str) -> bool {
    zeroship_auth::store::relay::resolve_active_alias(state.auth_pg.as_ref(), relay_email)
        .await
        .expect("resolve_active_alias")
        .is_some()
}

async fn identity_revoked_at_is_set(state: &AppState, client_id: &str, user_id: Uuid) -> bool {
    let rows = state
        .auth_pg
        .query(
            "SELECT revoked_at FROM auth.app_user_identities \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&client_id, &user_id],
        )
        .await
        .expect("query identity revoked_at");
    rows.first()
        .and_then(|r| r.get::<_, Option<chrono::DateTime<Utc>>>("revoked_at"))
        .is_some()
}

async fn cleanup_identities(state: &AppState, client_id: &str) {
    let _ = state
        .auth_pg
        .execute(
            "DELETE FROM auth.app_user_identities WHERE app_client_id = $1",
            &[&client_id],
        )
        .await;
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

async fn audit_event_count(
    state: &AppState,
    user_id: Uuid,
    event_type: &str,
    client_id: &str,
) -> i64 {
    let rows = state
        .auth_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM auth.audit_events \
             WHERE user_id = $1 AND event_type = $2 AND client_id = $3",
            &[&user_id, &event_type, &client_id],
        )
        .await
        .expect("count audit events");
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
    assert_eq!(
        audit_event_count(&fx.state, pat.user_id, "oauth_grant_revoke", &client_id).await,
        1
    );

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

/// 5c §6 — the B4 revocation cascade: revoking a grant sets
/// `app_user_identities.revoked_at` AND a subsequent inbound to that alias
/// bounces (the real 5b `resolve_active_alias` gate now returns `None`). The
/// DELETE + UPDATE commit atomically (BEGIN/COMMIT) on control's existing
/// `auth_pg` connection — no per-call connect. This is the full faithful loop:
/// the relay alias was forwarding (active) → revoke → it bounces (inactive).
#[compio::test]
async fn revoke_cascade_revokes_relay_alias_so_inbound_bounces() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "cascade").await;
    let pat = account_pat(&fx.state, "cascade").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-cascade-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, pat.user_id).await;
    insert_grant(&fx.state, pat.user_id, &client_id, &["apps:read", "email"]).await;
    let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    insert_identity_with_alias(&fx.state, &client_id, pat.user_id, &relay_email).await;

    // Pre-condition: the alias forwards (active map present — what 5b resolves).
    assert!(
        alias_is_active(&fx.state, &relay_email).await,
        "alias must be active (forwarding) BEFORE revoke"
    );

    // Revoke via the REAL control HTTP handler (runs the §6 cascade).
    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // The grant is gone AND the alias is revoked — committed atomically.
    assert_eq!(count_grant(&fx.state, pat.user_id, &client_id).await, 0);
    assert!(
        identity_revoked_at_is_set(&fx.state, &client_id, pat.user_id).await,
        "revoke must set app_user_identities.revoked_at (the cascade UPDATE)"
    );
    // The faithful seam: the 5b webhook's active-map resolution now returns
    // None ⇒ inbound to this alias BOUNCES (revoked/unknown-alias branch, §8).
    assert!(
        !alias_is_active(&fx.state, &relay_email).await,
        "after revoke, inbound to the alias must bounce (resolve_active_alias → None)"
    );

    cleanup_identities(&fx.state, &client_id).await;
    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

/// 5c §6.1 — re-grant stability: revoke then re-grant reuses the SAME alias
/// (Apple Hide-My-Email model). After re-grant `revoked_at` is cleared and the
/// alias forwards again — no new alias, no dead-alias bounce. The auth-side
/// writer (`mint_alias_at_consent`) clears `revoked_at` on the deterministic
/// row; here we exercise that clear directly to prove the row is reusable.
#[compio::test]
async fn re_grant_reuses_same_alias_with_cleared_revoked_at() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "regrant").await;
    let pat = account_pat(&fx.state, "regrant").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-grant-regrant-{}", Uuid::new_v4().simple());
    insert_client(&fx.state, &client_id, pat.user_id).await;
    insert_grant(&fx.state, pat.user_id, &client_id, &["email"]).await;
    let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    insert_identity_with_alias(&fx.state, &client_id, pat.user_id, &relay_email).await;

    // Revoke → alias goes inactive.
    let req = test::TestRequest::delete()
        .uri(&format!("/me/oauth-grants/{client_id}"))
        .header("authorization", pat.bearer())
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );
    assert!(!alias_is_active(&fx.state, &relay_email).await);

    // Re-grant: the auth-side writer clears revoked_at on the SAME row (no new
    // alias). `mint_alias_at_consent` does exactly this COALESCE/un-revoke.
    let reused = zeroship_auth::store::relay::mint_alias_at_consent(
        fx.state.auth_pg.as_ref(),
        &client_id,
        pat.user_id,
        "relay.zeroship.localhost",
    )
    .await
    .expect("re-grant mint");
    assert_eq!(
        reused.as_deref(),
        Some(relay_email.as_str()),
        "re-grant must reuse the SAME alias (Apple Hide-My-Email), not mint a new one"
    );
    assert!(
        alias_is_active(&fx.state, &relay_email).await,
        "after re-grant the alias forwards again (revoked_at cleared)"
    );

    cleanup_identities(&fx.state, &client_id).await;
    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

/// 5c §6 — the app-delete companion revokes ALL of an app's aliases. There is
/// NO cross-schema FK from `auth.app_user_identities` to control, so this
/// companion UPDATE (keyed on `client_id_for_app(uuid)`) is the SOLE guard
/// against orphaned live aliases. A dropped companion statement fails this test.
#[compio::test]
async fn app_delete_revokes_all_relay_aliases() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "appdel").await;
    // Two users with aliases on the SAME app (same client_id).
    let user_a = insert_user(&fx.state, "appdel-a").await;
    let user_b = insert_user(&fx.state, "appdel-b").await;
    let app_uuid = Uuid::new_v4();
    let client_id = zeroship_control::app_oauth_client::client_id_for_app(&app_uuid);
    let alias_a = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    let alias_b = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    insert_identity_with_alias(&fx.state, &client_id, user_a, &alias_a).await;
    insert_identity_with_alias(&fx.state, &client_id, user_b, &alias_b).await;

    assert!(alias_is_active(&fx.state, &alias_a).await);
    assert!(alias_is_active(&fx.state, &alias_b).await);

    // Drive the companion directly (the same call delete_app makes after the
    // control-schema cascade — keyed on the deterministic oac_ client_id).
    let revoked = zeroship_control::relay_revoke::revoke_all_aliases_for_client(
        &fx.state.auth_db_url,
        &client_id,
    )
    .await
    .expect("app-delete companion");
    assert_eq!(revoked, 2, "must revoke BOTH users' aliases for the app");

    assert!(
        !alias_is_active(&fx.state, &alias_a).await,
        "user A's alias must bounce after app delete"
    );
    assert!(
        !alias_is_active(&fx.state, &alias_b).await,
        "user B's alias must bounce after app delete"
    );

    cleanup_identities(&fx.state, &client_id).await;
    cleanup_user(&fx.state, user_a).await;
    cleanup_user(&fx.state, user_b).await;
}

/// Isolation regression (review BLOCKER + MAJOR finding 2) — the relay cascade
/// must run its `BEGIN…COMMIT` on a DEDICATED connection, never on the shared
/// `auth_pg`. The rejected design multiplexed the transaction onto `auth_pg`,
/// the single `Arc<Client>` every other control handler pipelines onto with NO
/// transaction isolation. That had two fatal symptoms this test pins:
///
///   1. **Head-of-line blocking.** While the cascade transaction is open, a
///      concurrent statement on `auth_pg` sits behind it in the connection's
///      FIFO — on the shared design the bystander write below would BLOCK until
///      the cascade's transaction resolved. On the dedicated design it returns
///      immediately.
///   2. **Transactional capture.** A bystander autocommit write physically
///      written between the cascade's BEGIN and its COMMIT/ROLLBACK would be
///      committed/rolled-back WITH the cascade. On the dedicated design it is
///      its own autocommit and survives the cascade's outcome unconditionally.
///
/// We hold the cascade transaction open deterministically by pre-locking (from
/// a third connection) the `control.oauth_grants` row the cascade's first
/// statement (DELETE) must touch, so the cascade blocks mid-transaction. While
/// it is blocked we issue a bystander write on `auth_pg` and assert it returns
/// promptly and persists — then release the lock and let the cascade finish.
/// On the shared-connection design this test deadlocks/blocks (symptom 1) and
/// the bystander write would be inside the cascade transaction (symptom 2).
#[compio::test]
async fn cascade_does_not_block_or_capture_concurrent_auth_pg_writes() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "isolation-regress").await;

    // The user whose grant the cascade revokes, plus a seeded grant row + alias.
    let user = insert_user(&fx.state, "isolation-cascade").await;
    let app_uuid = Uuid::new_v4();
    let client_id = zeroship_control::app_oauth_client::client_id_for_app(&app_uuid);
    insert_client(&fx.state, &client_id, user).await;
    insert_grant(&fx.state, user, &client_id, &["email"]).await;
    let alias = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    insert_identity_with_alias(&fx.state, &client_id, user, &alias).await;

    // An independent bystander identity + active alias the bystander write
    // (issued on the SHARED auth_pg) will revoke. Distinct row from the
    // cascade's — so the only way it could be affected is transactional capture.
    let bystander_user = insert_user(&fx.state, "isolation-bystander").await;
    let bystander_client = zeroship_control::app_oauth_client::client_id_for_app(&Uuid::new_v4());
    let bystander_alias = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    insert_identity_with_alias(&fx.state, &bystander_client, bystander_user, &bystander_alias)
        .await;

    // Third connection: hold a lock on the cascade's target grant row so the
    // cascade's DELETE blocks, keeping its transaction OPEN for the window
    // below. (A separate client, NOT auth_pg.)
    let (locker, locker_conn) = connect(&db_url, NoTls).await.expect("locker connect");
    compio::runtime::spawn(async move {
        let _ = locker_conn.run().await;
    })
    .detach();
    locker.execute("BEGIN", &[]).await.expect("locker begin");
    locker
        .execute(
            "SELECT 1 FROM control.oauth_grants \
             WHERE user_id = $1 AND client_id = $2 FOR UPDATE",
            &[&user, &client_id],
        )
        .await
        .expect("locker holds the grant row");

    // Kick off the cascade on its DEDICATED connection. It will BEGIN then block
    // on DELETE (the row is locked). We do NOT await it yet.
    let auth_db_url = fx.state.auth_db_url.clone();
    let cascade_client_id = client_id.clone();
    let cascade = compio::runtime::spawn(async move {
        zeroship_control::relay_revoke::revoke_grant_cascade(
            &auth_db_url,
            &user,
            &cascade_client_id,
        )
        .await
    });

    // Give the cascade a moment to open its transaction and block on the lock.
    compio::time::sleep(std::time::Duration::from_millis(200)).await;

    // While the cascade transaction is OPEN (blocked), a bystander write on the
    // SHARED auth_pg must return PROMPTLY (no head-of-line blocking) — on the
    // rejected shared-connection design this would deadlock behind the cascade.
    let bystander = compio::time::timeout(
        std::time::Duration::from_secs(5),
        fx.state.auth_pg.execute(
            "UPDATE auth.app_user_identities SET revoked_at = now() \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&bystander_client, &bystander_user],
        ),
    )
    .await
    .expect("bystander write must NOT block behind the cascade transaction")
    .expect("bystander autocommit write on auth_pg");
    assert_eq!(bystander, 1, "bystander revoked exactly its own alias row");

    // Release the lock so the cascade can complete.
    locker.execute("COMMIT", &[]).await.expect("locker commit");
    let revoked = cascade.await.expect("cascade joins");
    revoked.expect("cascade succeeds once the lock is released");

    // The bystander write was its own autocommit on auth_pg — it is committed
    // and visible REGARDLESS of the cascade's transaction. (Transactional
    // capture on the shared design would have tied it to the cascade.)
    assert!(
        !alias_is_active(&fx.state, &bystander_alias).await,
        "the bystander's autocommit revoke must persist independently of the cascade"
    );
    // And the cascade revoked its OWN alias (its transaction committed cleanly).
    assert!(
        !alias_is_active(&fx.state, &alias).await,
        "the cascade revoked its own alias after the lock released"
    );

    drop(locker);
    cleanup_identities(&fx.state, &client_id).await;
    cleanup_identities(&fx.state, &bystander_client).await;
    fx.cleanup_clients(&[client_id]).await;
    cleanup_user(&fx.state, user).await;
    cleanup_user(&fx.state, bystander_user).await;
}

/// 5c §6 (review MAJOR) — when the app-delete relay companion FAILS, `delete_app`
/// must NOT silently return 200: the app row is already gone (a retry is a 404
/// no-op) and there is no background sweep, so a swallowed failure permanently
/// strands LIVE aliases forwarding real mail. The handler surfaces the failure
/// as a 500 carrying `{deleted: true, aliases_revoked: false, client_id}` so the
/// operator can retry the revoke out-of-band. Pre-fix this returned 200.
///
/// We force ONLY the companion to fail by pointing the fixture's `auth_db_url`
/// (the URL the DEDICATED relay client connects on) at an unreachable address,
/// while the registry + `auth_pg` keep using the real DB so the authz guard and
/// the control-schema delete still succeed. This HTTP-level failure-arm
/// assertion is possible precisely because the cascade uses a dedicated
/// connection — killing it does not also kill the guard's `auth_pg`.
#[compio::test]
async fn app_delete_surfaces_companion_failure_as_500() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    // Registry/auth_pg = real DB; auth_db_url = unreachable ⇒ companion connect fails.
    let fx = Fixture::new_with_auth_db_url(
        &db_url,
        "postgres://nobody@127.0.0.1:1/nodb",
        "companion-fail",
    )
    .await;

    // A real app row so the registry cascade succeeds (delete returns Ok(true)).
    let app_name = format!("companionfail{}", Uuid::new_v4().simple());
    let record = fx
        .state
        .registry
        .create_app(&app_name, "free")
        .await
        .expect("create app");
    let app_id = record.id;
    let client_id = zeroship_control::app_oauth_client::client_id_for_app(&app_id);

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .service(web::resource("/api/apps/{id}").route(web::delete().to(api::delete_app))),
    )
    .await;

    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let req = test::TestRequest::delete()
        .uri(&format!("/api/apps/{app_id}"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    // The privacy-critical assertion: a failed companion is NOT a 200.
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a failed relay-alias companion must surface as 500, not a silent 200"
    );
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("delete body json");
    assert_eq!(
        body.get("deleted").and_then(Value::as_bool),
        Some(true),
        "the app IS deleted (registry cascade succeeded)"
    );
    assert_eq!(
        body.get("aliases_revoked").and_then(Value::as_bool),
        Some(false),
        "aliases were NOT revoked — caller must know they may still forward"
    );
    assert_eq!(
        body.get("client_id").and_then(Value::as_str),
        Some(client_id.as_str()),
        "the deterministic client_id is returned so the operator can retry out-of-band"
    );

    pat.cleanup(&fx.state).await;
}

/// 5c §6 (review MAJOR, success arm) — the happy path still returns 200
/// `{deleted: true}` when the companion succeeds (no aliases to revoke is also
/// success). This pins the 500 above to the FAILURE arm only.
#[compio::test]
async fn app_delete_returns_200_when_companion_succeeds() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_grants_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    // auth_db_url == real DB ⇒ companion connects + runs (revokes 0 rows = ok).
    let fx = Fixture::new(&db_url, "companion-ok").await;

    let app_name = format!("companionok{}", Uuid::new_v4().simple());
    let record = fx
        .state
        .registry
        .create_app(&app_name, "free")
        .await
        .expect("create app");
    let app_id = record.id;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .service(web::resource("/api/apps/{id}").route(web::delete().to(api::delete_app))),
    )
    .await;

    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let req = test::TestRequest::delete()
        .uri(&format!("/api/apps/{app_id}"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "companion success ⇒ 200 (the 500 is the failure arm only)"
    );
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("delete body json");
    assert_eq!(body.get("deleted").and_then(Value::as_bool), Some(true));

    pat.cleanup(&fx.state).await;
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
