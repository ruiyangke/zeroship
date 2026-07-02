//! Live-PG regression tests for admin OAuth client handlers.
//!
//! The control handler exercises the real authz extractor, PAT verifier, auth
//! migrations, and native `zeroship.oauth_clients` table.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use compio_postgres::{connect, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_authz::{policy_hash, Action, Effect, Policy, Resource, Statement};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    oauth_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
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
        "zs-oauth-handlers-{label}-{}",
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

impl Fixture {
    async fn new(db_url: &str, label: &str) -> Self {
        let (control_pg_client, control_pg_conn) = connect(db_url, NoTls).await.expect("control-pg connect");
        compio::runtime::spawn(async move {
            let _ = control_pg_conn.run().await;
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
            app_base_domain: "zeroship.localhost".to_string(),
            // The compiled default is now EMPTY (fail-closed), so seed an
            // explicit trusted client id for the "skip_consent derived from
            // whitelist" test. `acme-ci` is intentionally NOT listed, so the
            // untrusted-client test still gets skip_consent=false.
            trusted_oauth_clients: ["zeroship-builder".to_string()].into_iter().collect(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(zeroship_authn::PatIssuer::dev_insecure()),
            auth_provider: zeroship_control::platform_auth_provider("https://auth.zeroship.test/oauth2", Some("http://127.0.0.1:9/oauth2/.well-known/jwks.json".to_string())),
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

        Self {
            state,
            blob_root,
            deploy_tmp_dir,
        }
    }

    async fn cleanup_clients(&self, client_ids: &[String]) {
        if client_ids.is_empty() {
            return;
        }
        let ids: Vec<&str> = client_ids.iter().map(String::as_str).collect();
        let _ = self
            .state
            .control_pg
            .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = ANY($1)", &[&ids])
            .await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

struct NonAdminPat {
    user_id: Uuid,
    token_id: Uuid,
    token: String,
}

impl NonAdminPat {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    async fn cleanup(&self, state: &AppState) {
        let _ = state
            .control_pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE token_id = $1 OR actor_user_id = $2",
                &[&self.token_id, &self.user_id],
            )
            .await;
        let _ = state
            .control_pg
            .execute(
                "DELETE FROM zeroship.permission_tokens WHERE id = $1",
                &[&self.token_id],
            )
            .await;
        let _ = state
            .control_pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id])
            .await;
    }
}

async fn non_admin_pat(state: &AppState) -> NonAdminPat {
    let user_id = Uuid::new_v4();
    let email = format!("non-admin-oauth-{user_id}@zeroship.test");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'Non Admin OAuth Test User', NOW())",
            &[&user_id, &email],
        )
        .await
        .expect("insert non-admin user");

    let token_id = Uuid::new_v4();
    let policies = platform_policy().to_json_value();
    let hash = policy_hash(&policies);
    let expires_at = Utc::now() + Duration::days(1);
    let token = state
        .pat_issuer
        .issue(token_id, user_id, hash.clone(), expires_at)
        .expect("issue non-admin PAT");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.permission_tokens \
                (id, owner_id, kind, name, policies, policy_hash, expires_at) \
             VALUES ($1, $2, 'pat', 'integration non-admin PAT', $3, $4, $5)",
            &[&token_id, &user_id, &policies, &hash, &expires_at],
        )
        .await
        .expect("insert non-admin PAT row");

    NonAdminPat {
        user_id,
        token_id,
        token,
    }
}

fn platform_policy() -> Policy {
    Policy {
        name: "oauth client admin".to_string(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::PlatformPoliciesWrite],
            resources: vec![Resource::Any],
            conditions: Vec::new(),
        }],
    }
}

fn client_body(client_id: &str) -> Value {
    json!({
        "client_id": client_id,
        "client_name": "ACME CI",
        "client_uri": "https://acme.example",
        "logo_uri": "https://acme.example/logo.png",
        "redirect_uris": ["https://ci.acme.example/oidc/callback"],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "scope": "apps:read apps:deploy env:read",
        "token_endpoint_auth_method": "client_secret_basic"
    })
}

async fn count_client(state: &AppState, client_id: &str) -> i64 {
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("count oauth client");
    rows[0].get("n")
}

async fn audit_event_count(
    state: &AppState,
    user_id: Uuid,
    event_type: &str,
    client_id: &str,
) -> i64 {
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM zeroship.audit_events \
             WHERE actor_user_id = $1 AND event_type = $2 AND client_id = $3",
            &[&user_id, &event_type, &client_id],
        )
        .await
        .expect("count audit events");
    rows[0].get("n")
}

async fn persisted_skip_consent(state: &AppState, client_id: &str) -> bool {
    let rows = state
        .control_pg
        .query(
            "SELECT skip_consent FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("select oauth client skip_consent");
    rows[0].get("skip_consent")
}

async fn oauth_client_row(state: &AppState, client_id: &str) -> compio_postgres::Row {
    let rows = state
        .control_pg
        .query(
            "SELECT client_name, client_uri, logo_uri, redirect_uris, scopes, \
                    skip_consent, client_secret_hash, refresh_allowed, \
                    token_endpoint_auth_method \
             FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("select oauth client");
    assert_eq!(rows.len(), 1, "oauth_clients row exists for {client_id}");
    rows.into_iter().next().expect("one row")
}

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(
            web::App::new()
                .state($fx.state.clone())
                .configure(oauth_handlers::configure),
        )
        .await
    }};
}

#[compio::test]
async fn unauthenticated_request_returns_401() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "unauth").await;
    let app = init_control!(fx);
    let client_id = format!("oauth-unauth-{}", Uuid::new_v4().simple());

    let req = test::TestRequest::post()
        .uri("/admin/oauth-clients")
        .set_json(&client_body(&client_id))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[compio::test]
async fn non_admin_request_returns_403() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "non-admin").await;
    let pat = non_admin_pat(&fx.state).await;
    let app = init_control!(fx);
    let client_id = format!("oauth-non-admin-{}", Uuid::new_v4().simple());

    let req = test::TestRequest::post()
        .uri("/admin/oauth-clients")
        .header("authorization", pat.bearer())
        .set_json(&client_body(&client_id))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn admin_can_register_oauth_client_in_native_store() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "register").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let app = init_control!(fx);
    let client_id = format!("oauth-register-{}", Uuid::new_v4().simple());

    let req = test::TestRequest::post()
        .uri("/admin/oauth-clients")
        .header("authorization", pat.bearer())
        .set_json(&client_body(&client_id))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("response json");
    assert_eq!(body["client_id"].as_str(), Some(client_id.as_str()));
    let secret = body["client_secret"].as_str().expect("client_secret");
    assert_eq!(secret.len(), 64);
    assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(body["client_secret_show_once"], true);
    assert_eq!(body["scopes"], json!(["apps:read", "apps:deploy", "env:read"]));
    assert_eq!(count_client(&fx.state, &client_id).await, 1);
    assert_eq!(
        audit_event_count(&fx.state, pat.user_id, "oauth_client_create", &client_id).await,
        1
    );

    let row = oauth_client_row(&fx.state, &client_id).await;
    assert_eq!(row.get::<_, String>("client_name"), "ACME CI");
    assert_eq!(
        row.get::<_, Option<String>>("client_uri").as_deref(),
        Some("https://acme.example")
    );
    assert_eq!(
        row.get::<_, Option<String>>("logo_uri").as_deref(),
        Some("https://acme.example/logo.png")
    );
    assert_eq!(
        row.get::<_, Vec<String>>("redirect_uris"),
        vec!["https://ci.acme.example/oidc/callback".to_string()]
    );
    assert_eq!(
        row.get::<_, Vec<String>>("scopes"),
        vec![
            "apps:read".to_string(),
            "apps:deploy".to_string(),
            "env:read".to_string()
        ]
    );
    assert!(!row.get::<_, bool>("skip_consent"));
    assert!(row.get::<_, bool>("refresh_allowed"));
    assert_eq!(
        row.get::<_, String>("token_endpoint_auth_method"),
        "client_secret_basic"
    );
    let hash = row
        .get::<_, Option<String>>("client_secret_hash")
        .expect("client_secret_hash");
    assert!(
        zeroship_core::auth::validate_api_key(secret, &hash),
        "client_secret_hash validates the generated one-time secret"
    );

    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn skip_consent_is_derived_from_whitelist_not_body() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "trusted-client").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let app = init_control!(fx);
    let client_id = "zeroship-builder".to_string();
    fx.cleanup_clients(std::slice::from_ref(&client_id)).await;

    let req = test::TestRequest::post()
        .uri("/admin/oauth-clients")
        .header("authorization", pat.bearer())
        .set_json(&client_body(&client_id))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(persisted_skip_consent(&fx.state, &client_id).await);

    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn arbitrary_client_gets_skip_consent_false() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "untrusted-client").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let app = init_control!(fx);
    let client_id = "acme-ci".to_string();
    fx.cleanup_clients(std::slice::from_ref(&client_id)).await;

    let req = test::TestRequest::post()
        .uri("/admin/oauth-clients")
        .header("authorization", pat.bearer())
        .set_json(&client_body(&client_id))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(!persisted_skip_consent(&fx.state, &client_id).await);

    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn invalid_scope_returns_400() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "invalid-scope").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let app = init_control!(fx);
    let client_id = format!("oauth-invalid-scope-{}", Uuid::new_v4().simple());
    let mut body = client_body(&client_id);
    body["scope"] = Value::String("apps:read bogus:scope".to_string());

    let req = test::TestRequest::post()
        .uri("/admin/oauth-clients")
        .header("authorization", pat.bearer())
        .set_json(&body)
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(count_client(&fx.state, &client_id).await, 0);
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn duplicate_client_id_returns_409() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "duplicate").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let app = init_control!(fx);
    let client_id = format!("oauth-duplicate-{}", Uuid::new_v4().simple());

    let req = test::TestRequest::post()
        .uri("/admin/oauth-clients")
        .header("authorization", pat.bearer())
        .set_json(&client_body(&client_id))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = test::TestRequest::post()
        .uri("/admin/oauth-clients")
        .header("authorization", pat.bearer())
        .set_json(&client_body(&client_id))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(count_client(&fx.state, &client_id).await, 1);

    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn list_returns_registered_clients() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "list").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let app = init_control!(fx);
    let client_id = format!("oauth-list-{}", Uuid::new_v4().simple());
    let redirect_uris = vec!["https://list.example/callback"];
    let scopes = vec!["apps:read", "apps:deploy"];
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                 skip_consent, created_by, hydra_client_id) \
             VALUES ($1, 'List Client', NULL, NULL, $2, $3, true, $4, $1)",
            &[&client_id, &redirect_uris, &scopes, &pat.user_id],
        )
        .await
        .expect("seed oauth client");

    let req = test::TestRequest::get()
        .uri("/admin/oauth-clients")
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    let clients = body.as_array().expect("clients array");
    let client = clients
        .iter()
        .find(|client| client["client_id"].as_str() == Some(client_id.as_str()))
        .expect("registered client listed");
    assert_eq!(client["client_name"], "List Client");
    assert_eq!(client["redirect_uris"], json!(["https://list.example/callback"]));
    assert_eq!(client["scopes"], json!(["apps:read", "apps:deploy"]));
    assert_eq!(client["skip_consent"], true);
    assert!(client.get("client_secret").is_none());

    fx.cleanup_clients(&[client_id]).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn delete_removes_from_native_store() {
    let Some(db_url) = db_url() else {
        eprintln!("[oauth_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "delete").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let app = init_control!(fx);
    let client_id = format!("oauth-delete-{}", Uuid::new_v4().simple());

    let req = test::TestRequest::post()
        .uri("/admin/oauth-clients")
        .header("authorization", pat.bearer())
        .set_json(&client_body(&client_id))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        audit_event_count(&fx.state, pat.user_id, "oauth_client_create", &client_id).await,
        1
    );

    let req = test::TestRequest::delete()
        .uri(&format!("/admin/oauth-clients/{client_id}"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(count_client(&fx.state, &client_id).await, 0);
    assert_eq!(
        audit_event_count(&fx.state, pat.user_id, "oauth_client_delete", &client_id).await,
        1
    );

    pat.cleanup(&fx.state).await;
}
