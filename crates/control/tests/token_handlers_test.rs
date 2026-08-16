//! HTTP regression tests for creator-console PAT handlers.
//!
//! Under the R5 cutover the bespoke console-session principal path is gone:
//! the control plane authenticates every request through the `AuthzGuard`
//! bearer path. PAT minting still requires an INTERACTIVE (non-PAT) principal,
//! which is now an OAuth/BFF session access token (the `oauth_guard_from_bearer`
//! arm: `token_id == None`). So these tests drive the handlers with a signed
//! platform access token rather than a console-session cookie.

use std::path::PathBuf;
use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;

use zeroship_auth::{cron::account_reaper, store::users};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};

mod common;

fn db_url() -> String {
    zeroship_core::test_env!("AUTH_DB_URL")
        .or_else(|| zeroship_core::test_env!("PG_TEST_URL"))
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| {
            "postgresql://postgres:zeroship@localhost:5440/zeroship_billing_test".to_string()
        })
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
    _jwks: common::PlatformJwks,
}

impl Fixture {
    async fn new(db_url: &str, label: &str, platform_role: Option<&str>) -> Self {
        let blob_root = tmpdir(&format!("blob-{label}"));
        let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));

        let registry = Registry::new(db_url).await.expect("registry");

        let env_store = EnvStore::new(registry.clone(), "test-master-key")
            .expect("env store");
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

        // The acting creator. Created BEFORE the mock introspector so its `sub`
        // resolves to this user; its platform role drives the grant ceiling.
        let user_id = Uuid::new_v4();
        let email = format!("{label}-{user_id}@zeroship.test");
        control_pg
            .execute(
                "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
                &[&user_id, &email, &label],
            )
            .await
            .expect("insert user");
        if let Some(role) = platform_role {
            control_pg
                .execute(
                    "INSERT INTO zeroship.platform_admin_roles (user_id, role) VALUES ($1, $2)",
                    &[&user_id, &role],
                )
                .await
                .expect("insert platform role");
        }

        let jwks = common::PlatformJwks::start();

        let state = Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            blob_store,
            workflow_blob_store,
            control_key: SecretString::new("test-control-key".to_string()),
            auth_platform_mint_key: SecretString::new("test-platform-mint-key".to_string()),
            master_key: SecretString::new("test-master-key".to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".to_string(),
            gateway_url: "http://127.0.0.1:9".to_string(),
            migrated_url: "http://127.0.0.1:9".to_string(),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg,
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(zeroship_authn::PatIssuer::generate_ephemeral()),
            auth_provider: common::platform_auth_provider(jwks.jwks_url()),
        // No platform deploy-token mint here: that is control's OUTBOUND
        // destination for the device flow, and no fixture below drives one.
        platform_mint_url: None,
        provider_registry: zeroship_control::metering::provider::builtin_registry(),
        billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
        billing_stream: None,
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
            user_id,
            blob_root,
            deploy_tmp_dir,
            _jwks: jwks,
        }
    }

    async fn cleanup(&self) {
        let token_rows = self
            .state
            .control_pg
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
                .control_pg
                .execute("DELETE FROM zeroship.authz_decisions WHERE token_id = $1", &[&id])
                .await;
        }
        let _ = self
            .state
            .control_pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = self
            .state
            .control_pg
            .execute(
                "DELETE FROM zeroship.permission_tokens WHERE owner_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = self
            .state
            .control_pg
            .execute(
                "DELETE FROM zeroship.app_members WHERE user_id = $1",
                &[&self.user_id],
            )
            .await;
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
        .control_pg
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
    ($app:expr, $user_id:expr, $name:expr) => {{
        let body = json!({
            "name": $name,
            "policies": deploy_policy(),
            "expires_in_days": 90
        });
        let req = test::TestRequest::post()
            .uri("/me/tokens")
            .header("accept", "application/json")
            .header(
                "authorization",
                common::platform_bearer($user_id, "apps:read apps:deploy"),
            )
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
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "valid", Some("admin")).await;
    let app = init_control!(fx);

    let json = create_pat!(app, fx.user_id, "CI deploy");
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

    // Teardown: the service and the fixture both hold connections, and locals
    // are dropped only after the body returns - by which point the runtime is
    // gone and the sockets can no longer be closed. Drop them explicitly, then
    // wait for the close to land.
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn create_pat_with_policy_exceeding_user_returns_400() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "viewer", Some("readonly")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", common::platform_bearer(fx.user_id, "apps:read apps:deploy"))
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

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn app_owner_can_create_any_resource_pat_for_owned_action() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "owner-any", None).await;
    // The owned resource must be a REAL app row: `app_members.app_id` is a
    // `uuid` column with an FK into `zeroship.apps(id)`, and `apps.plan_id`
    // FKs into the `zeroship.plans` catalog. Seed the built-in plans, then go
    // through `Registry::create_app`, which writes the app + the owner
    // `app_members` row in one transaction (binding `fx.user_id` as owner) —
    // the canonical path, instead of hand-inserting a bogus `"app-<uuid>"`
    // string into the uuid column (the old fixture's WrongType bug).
    common::ensure_builtin_plans(&fx.state.registry).await;
    let app_name = format!("owner-any-{}", Uuid::new_v4().simple());
    let owned = fx
        .state
        .registry
        .create_app(
            &app_name,
            &zeroship_control::plan_catalog::free_plan_id(),
            &fx.user_id,
        )
        .await
        .expect("create owned app (seeds app + owner app_members atomically)");
    let app = init_control!(fx);

    let created = create_pat!(app, fx.user_id, "owner deploy");
    assert!(created.get("token").and_then(Value::as_str).is_some());

    // Drop the owned app (CASCADE removes its `app_members` row) before the
    // generic fixture teardown deletes the user.
    fx.state
        .control_pg
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&owned.id])
        .await
        .expect("delete owned app");
    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn create_pat_with_invalid_resource_id_returns_400() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "invalid-resource", Some("admin")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", common::platform_bearer(fx.user_id, "apps:read apps:deploy"))
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

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn create_pat_with_empty_statement_actions_returns_400() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "empty-actions", Some("admin")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", common::platform_bearer(fx.user_id, "apps:read apps:deploy"))
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

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn create_pat_with_empty_statement_resources_returns_400() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "empty-resources", Some("admin")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", common::platform_bearer(fx.user_id, "apps:read apps:deploy"))
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

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn create_pat_with_mfa_condition_returns_400() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "mfa-condition", Some("admin")).await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", common::platform_bearer(fx.user_id, "apps:read apps:deploy"))
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

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn list_pats_returns_user_tokens_without_secret() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "list", Some("admin")).await;
    let app = init_control!(fx);

    create_pat!(app, fx.user_id, "CI deploy A");
    create_pat!(app, fx.user_id, "CI deploy B");

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", common::platform_bearer(fx.user_id, "apps:read apps:deploy"))
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

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn delete_pat_marks_revoked() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "delete", Some("admin")).await;
    let app = init_control!(fx);

    let created = create_pat!(app, fx.user_id, "CI deploy");
    let id = created.get("id").and_then(Value::as_str).expect("id");
    let req = test::TestRequest::delete()
        .uri(&format!("/me/tokens/{id}"))
        .header("accept", "application/json")
        .header("authorization", common::platform_bearer(fx.user_id, "apps:read apps:deploy"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = test::read_body(resp).await;
    let deleted: Value = serde_json::from_slice(&bytes).expect("delete JSON");
    assert!(deleted.get("revoked_at").and_then(Value::as_str).is_some());

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", common::platform_bearer(fx.user_id, "apps:read apps:deploy"))
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

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn using_revoked_pat_returns_401() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "revoked-use", Some("admin")).await;
    let app = init_control!(fx);
    let pat = common::authz_fixture::admin_pat(&fx.state).await;

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", pat.bearer())
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::OK);

    fx.state
        .control_pg
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
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    pat.cleanup(&fx.state).await;
    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn pat_owned_by_anonymized_creator_returns_401() {
    let db_url = db_url();
    let fx = Fixture::new(&db_url, "erased-owner", Some("admin")).await;
    let app = init_control!(fx);
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let stripe_account_id = format!("acct_{}", Uuid::new_v4().simple());

    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.creator_accounts (creator_id, stripe_account_id) \
             VALUES ($1, $2)",
            &[&pat.user_id, &stripe_account_id],
        )
        .await
        .expect("insert retained creator account");
    let (mut deletion, deletion_driver) =
        compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("account deletion connection");
    compio::runtime::spawn(async move {
        if let Err(e) = deletion_driver.run().await {
            eprintln!("account deletion connection error: {e}");
        }
    })
    .detach();
    users::request_deletion(
        &mut deletion,
        pat.user_id,
        account_reaper::GRACE_DAYS,
    )
    .await
    .expect("request account deletion")
    .expect("PAT owner exists");
    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.users SET deletion_scheduled_for = NOW() - INTERVAL '1 second' \
             WHERE id = $1",
            &[&pat.user_id],
        )
        .await
        .expect("backdate account deletion");
    account_reaper::tick(&mut deletion)
        .await
        .expect("run account reaper");

    let owner = fx
        .state
        .control_pg
        .query_one(
            "SELECT anonymized_at FROM zeroship.users WHERE id = $1",
            &[&pat.user_id],
        )
        .await
        .expect("retained owner row");
    assert!(
        owner
            .get::<_, Option<chrono::DateTime<chrono::Utc>>>("anonymized_at")
            .is_some(),
        "the real reaper must anonymize the retained creator"
    );
    let retained_pat = fx
        .state
        .control_pg
        .query(
            "SELECT 1 FROM zeroship.permission_tokens WHERE id = $1",
            &[&pat.token_id],
        )
        .await
        .expect("query retained PAT");
    assert_eq!(retained_pat.len(), 1, "anonymization must retain the PAT row");

    let req = test::TestRequest::get()
        .uri("/me/tokens")
        .header("accept", "application/json")
        .header("authorization", pat.bearer())
        .to_request();
    let status = test::call_service(&app, req).await.status();

    fx.state
        .control_pg
        .execute(
            "DELETE FROM zeroship.creator_accounts WHERE creator_id = $1",
            &[&pat.user_id],
        )
        .await
        .expect("delete retained creator account");
    pat.cleanup(&fx.state).await;
    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
