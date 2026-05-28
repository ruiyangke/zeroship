//! Live-PG regression tests for platform admin handlers.
//!
//! Skips when no test Postgres URL is set. The handlers use AuthzGuard,
//! so the tests create real `auth.users`, `auth.console_sessions`,
//! `platform.roles`, permission tokens, and policy rows.

use std::path::PathBuf;
use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    admin_handlers, oidc_rp, token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};
use zeroship_core::oidc_verify::TokenClaims;

mod common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "zship-admin-handlers-{label}-{}",
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

async fn build_test_state(db_url: &str, label: &str) -> Fixture {
    let (auth_pg_client, auth_pg_conn) = connect(db_url, NoTls).await.expect("auth-pg connect");
    compio::runtime::spawn(async move {
        let _ = auth_pg_conn.run().await;
    })
    .detach();

    zeroship_auth::store::migrations::migrate(&auth_pg_client)
        .await
        .expect("auth migrations");

    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false)
        .expect("env store");
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
        hydra_admin_url: "http://127.0.0.1:4445".to_string(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(token_handlers::PatIssuer::dev_insecure()),
        hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
            "http://127.0.0.1:9",
        )),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
    }
}

async fn insert_user(pg: &Client, label: &str) -> Uuid {
    let email = format!(
        "{label}-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let rows = pg
        .query(
            "INSERT INTO auth.users (email, name, email_verified_at) \
             VALUES ($1, $2, NOW()) \
             RETURNING id",
            &[&email, &label],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

fn claims_for(user_id: Uuid) -> TokenClaims {
    TokenClaims {
        sub: user_id.to_string(),
        iss: "https://auth.zeroship.test/".to_string(),
        aud: serde_json::Value::String("console.zeroship.ai".to_string()),
        exp: 9_999_999_999,
        iat: 0,
        nbf: None,
        nonce: None,
        at_hash: None,
        c_hash: None,
        email: Some(format!("{user_id}@zeroship.test")),
        email_verified: Some(true),
        name: Some("Admin Handler User".to_string()),
        picture: None,
        acr: None,
        amr: None,
        other: Default::default(),
    }
}

async fn session_cookie(pg: &Client, user_id: Uuid) -> String {
    let session = zeroship_control::console_sessions::create(pg, &claims_for(user_id))
        .await
        .expect("create console session");
    oidc_rp::set_console_session_cookie(&session.id, false)
}

async fn count_role(pg: &Client, user_id: Uuid, role: &str) -> i64 {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM platform.roles \
             WHERE user_id = $1 AND role = $2",
            &[&user_id, &role],
        )
        .await
        .expect("count role");
    rows[0].get("n")
}

async fn count_audit(pg: &Client, event_type: &str, target: Uuid) -> i64 {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM auth.audit_events \
             WHERE event_type = $1 AND detail->>'target' = $2",
            &[&event_type, &target.to_string()],
        )
        .await
        .expect("count audit");
    rows[0].get("n")
}

async fn app_flag(pg: &Client, app_id: Uuid, column: &str) -> bool {
    let sql = match column {
        "audit_locked" => "SELECT audit_locked AS flag FROM apps WHERE id = $1",
        "suspended" => "SELECT suspended AS flag FROM apps WHERE id = $1",
        _ => panic!("unknown app flag column: {column}"),
    };
    let rows = pg
        .query(sql, &[&app_id])
        .await
        .expect("select app flag");
    rows[0].get("flag")
}

async fn cleanup_user(pg: &Client, user_id: Uuid) {
    let _ = pg
        .execute(
            "DELETE FROM control.authz_decisions WHERE user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = pg
        .execute("DELETE FROM auth.audit_events WHERE user_id = $1", &[&user_id])
        .await;
    let _ = pg
        .execute("DELETE FROM auth.console_sessions WHERE user_id = $1", &[&user_id])
        .await;
    let _ = pg
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user_id])
        .await;
}

fn valid_cedar_source() -> &'static str {
    r#"permit (
  principal is User,
  action == Action::"apps:read",
  resource
);"#
}

#[compio::test]
async fn non_admin_cannot_grant_platform_role() {
    let Some(db_url) = db_url() else {
        eprintln!("[admin_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = build_test_state(&db_url, "non-admin").await;
    let actor = insert_user(&fx.state.auth_pg, "non-admin-actor").await;
    let target = insert_user(&fx.state.auth_pg, "non-admin-target").await;
    let cookie = session_cookie(&fx.state.auth_pg, actor).await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(admin_handlers::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/admin/users/{target}/role"))
        .set_json(&serde_json::json!({"role": "admin"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        matches!(resp.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
        "unauthenticated admin role grant should be rejected"
    );

    let req = test::TestRequest::post()
        .uri(&format!("/admin/users/{target}/role"))
        .header("cookie", cookie)
        .set_json(&serde_json::json!({"role": "admin"}))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(count_role(&fx.state.auth_pg, target, "admin").await, 0);

    cleanup_user(&fx.state.auth_pg, actor).await;
    cleanup_user(&fx.state.auth_pg, target).await;
}

#[compio::test]
async fn admin_can_grant_platform_role() {
    let Some(db_url) = db_url() else {
        eprintln!("[admin_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = build_test_state(&db_url, "grant").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let target = insert_user(&fx.state.auth_pg, "grant-target").await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(admin_handlers::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri(&format!("/admin/users/{target}/role"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({"role": "support"}))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(count_role(&fx.state.auth_pg, target, "support").await, 1);
    assert_eq!(
        count_audit(&fx.state.auth_pg, "platform_role_granted", target).await,
        1
    );

    cleanup_user(&fx.state.auth_pg, target).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn admin_can_revoke_platform_role() {
    let Some(db_url) = db_url() else {
        eprintln!("[admin_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = build_test_state(&db_url, "revoke").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let target = insert_user(&fx.state.auth_pg, "revoke-target").await;
    fx.state
        .auth_pg
        .execute(
            "INSERT INTO platform.roles (user_id, role, granted_by) \
             VALUES ($1, 'support', $2)",
            &[&target, &pat.user_id],
        )
        .await
        .expect("seed support role");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(admin_handlers::configure),
    )
    .await;
    let req = test::TestRequest::delete()
        .uri(&format!("/admin/users/{target}/role"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(count_role(&fx.state.auth_pg, target, "support").await, 0);

    cleanup_user(&fx.state.auth_pg, target).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn admin_can_audit_lock_app() {
    let Some(db_url) = db_url() else {
        eprintln!("[admin_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = build_test_state(&db_url, "audit-lock").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("audit-lock-{}", Uuid::new_v4().simple()),
            "free",
        )
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(admin_handlers::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri(&format!("/admin/apps/{}/audit-lock", app_record.id))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({"audit_locked": true}))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(app_flag(&fx.state.auth_pg, app_record.id, "audit_locked").await);

    let _ = fx.state.registry.delete_app(&app_record.id).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn admin_can_suspend_app() {
    let Some(db_url) = db_url() else {
        eprintln!("[admin_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = build_test_state(&db_url, "suspend").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(&format!("suspend-{}", Uuid::new_v4().simple()), "free")
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(admin_handlers::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri(&format!("/admin/apps/{}/suspend", app_record.id))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({"suspended": true}))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(app_flag(&fx.state.auth_pg, app_record.id, "suspended").await);

    let _ = fx.state.registry.delete_app(&app_record.id).await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn admin_can_create_platform_policy_with_valid_cedar() {
    let Some(db_url) = db_url() else {
        eprintln!("[admin_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = build_test_state(&db_url, "policy-valid").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let policy_id = "lock_writes";
    let _ = fx
        .state
        .auth_pg
        .execute("DELETE FROM control.platform_policies WHERE id = $1", &[&policy_id])
        .await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(admin_handlers::configure),
    )
    .await;
    let req = test::TestRequest::put()
        .uri(&format!("/admin/platform-policies/{policy_id}"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({
            "cedar_source": valid_cedar_source(),
            "enabled": true
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let rows = fx
        .state
        .auth_pg
        .query(
            "SELECT cedar_source, enabled FROM control.platform_policies WHERE id = $1",
            &[&policy_id],
        )
        .await
        .expect("select policy");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>("cedar_source"), valid_cedar_source());
    assert!(rows[0].get::<_, bool>("enabled"));

    let _ = fx
        .state
        .auth_pg
        .execute("DELETE FROM control.platform_policies WHERE id = $1", &[&policy_id])
        .await;
    pat.cleanup(&fx.state).await;
}

#[compio::test]
async fn admin_cannot_create_platform_policy_with_invalid_cedar() {
    let Some(db_url) = db_url() else {
        eprintln!("[admin_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = build_test_state(&db_url, "policy-invalid").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let policy_id = "invalid_cedar";
    let _ = fx
        .state
        .auth_pg
        .execute("DELETE FROM control.platform_policies WHERE id = $1", &[&policy_id])
        .await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(admin_handlers::configure),
    )
    .await;
    let req = test::TestRequest::put()
        .uri(&format!("/admin/platform-policies/{policy_id}"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({
            "cedar_source": "this is not cedar",
            "enabled": true
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let rows = fx
        .state
        .auth_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM control.platform_policies WHERE id = $1",
            &[&policy_id],
        )
        .await
        .expect("count invalid policy");
    assert_eq!(rows[0].get::<_, i64>("n"), 0);

    pat.cleanup(&fx.state).await;
}
