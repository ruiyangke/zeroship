//! Live-PG regression tests for platform admin handlers.
//!
//! Skips when no test Postgres URL is set. The handlers use AuthzGuard,
//! so the tests create real `auth.users`, `platform.roles`, permission
//! tokens (the bearer principal path), and policy rows.

use std::path::PathBuf;
use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    admin_handlers, token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
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
    let (control_pg_client, control_pg_conn) = connect(db_url, NoTls).await.expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::bootstrap_console::seed_plans(&registry).await.expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false)
        .expect("env store");
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
        hydra_admin_url: "http://127.0.0.1:4445".to_string(),
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
            "INSERT INTO zeroship.users (email, name, email_verified_at) \
             VALUES ($1, $2, NOW()) \
             RETURNING id",
            &[&email, &label],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

async fn count_role(pg: &Client, user_id: Uuid, role: &str) -> i64 {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.platform_admin_roles \
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
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.audit_events \
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
            "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.audit_events WHERE actor_user_id = $1", &[&user_id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
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
    // A NON-admin actor (a creator who is not a platform admin) — bearer is the
    // only principal path now, so the actor is a non-admin PAT.
    let actor = common::authz_fixture::non_admin_pat(&fx.state).await;
    let target = insert_user(&fx.state.control_pg, "non-admin-target").await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(admin_handlers::configure),
    )
    .await;

    // (1) No bearer at all → rejected (401/403).
    let req = test::TestRequest::post()
        .uri(&format!("/admin/users/{target}/role"))
        .set_json(&serde_json::json!({"role": "admin"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        matches!(resp.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
        "unauthenticated admin role grant should be rejected"
    );

    // (2) Authenticated as a non-admin → 403 forbidden, no role written.
    let req = test::TestRequest::post()
        .uri(&format!("/admin/users/{target}/role"))
        .header("authorization", actor.bearer())
        .set_json(&serde_json::json!({"role": "admin"}))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(count_role(&fx.state.control_pg, target, "admin").await, 0);

    actor.cleanup(&fx.state).await;
    cleanup_user(&fx.state.control_pg, target).await;
}

#[compio::test]
async fn admin_can_grant_platform_role() {
    let Some(db_url) = db_url() else {
        eprintln!("[admin_handlers_test] AUTH_DB_URL not set - skipping");
        return;
    };
    let fx = build_test_state(&db_url, "grant").await;
    let pat = common::authz_fixture::admin_pat(&fx.state).await;
    let target = insert_user(&fx.state.control_pg, "grant-target").await;

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
    assert_eq!(count_role(&fx.state.control_pg, target, "support").await, 1);
    assert_eq!(
        count_audit(&fx.state.control_pg, "platform_role_granted", target).await,
        1
    );

    cleanup_user(&fx.state.control_pg, target).await;
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
    let target = insert_user(&fx.state.control_pg, "revoke-target").await;
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
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
    assert_eq!(count_role(&fx.state.control_pg, target, "support").await, 0);

    cleanup_user(&fx.state.control_pg, target).await;
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
            &zeroship_control::bootstrap_console::free_plan_id(),
            &pat.user_id,
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
    assert!(app_flag(&fx.state.control_pg, app_record.id, "audit_locked").await);

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
        .create_app(&format!("suspend-{}", Uuid::new_v4().simple()), &zeroship_control::bootstrap_console::free_plan_id(), &pat.user_id)
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
    assert!(app_flag(&fx.state.control_pg, app_record.id, "suspended").await);

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
        .control_pg
        .execute("DELETE FROM zeroship.platform_policies WHERE id = $1", &[&policy_id])
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
        .control_pg
        .query(
            "SELECT cedar_source, enabled FROM zeroship.platform_policies WHERE id = $1",
            &[&policy_id],
        )
        .await
        .expect("select policy");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>("cedar_source"), valid_cedar_source());
    assert!(rows[0].get::<_, bool>("enabled"));

    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.platform_policies WHERE id = $1", &[&policy_id])
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
        .control_pg
        .execute("DELETE FROM zeroship.platform_policies WHERE id = $1", &[&policy_id])
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
        .control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.platform_policies WHERE id = $1",
            &[&policy_id],
        )
        .await
        .expect("count invalid policy");
    assert_eq!(rows[0].get::<_, i64>("n"), 0);

    pat.cleanup(&fx.state).await;
}
