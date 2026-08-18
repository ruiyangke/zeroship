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
    admin_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};

mod common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> String {
    zeroship_core::test_env!("AUTH_DB_URL")
        .or_else(|| zeroship_core::test_env!("PG_TEST_URL"))
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| {
            "postgresql://postgres:zeroship@localhost:5440/zeroship_billing_test".to_string()
        })
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
    zeroship_control::plan_catalog::seed_plans(&registry).await.expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY)
        .expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
            workflow_blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
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
        control_pg: Arc::new(control_pg_client),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        auth_provider: zeroship_control::platform_auth_provider("https://auth.zeroship.test/oauth2", Some(common::platform_jwks_url())),
        // No platform deploy-token mint here: that is control's OUTBOUND
        // destination for the device flow, and no fixture below drives one.
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

#[compio::test]
async fn non_admin_cannot_grant_platform_role() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "non-admin").await;
    // A NON-admin actor (a creator who is not a platform admin) — bearer is the
    // only principal path now, so the actor is a non-admin PAT.
    let actor = common::authz_fixture::non_admin_principal(&fx.state).await;
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
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();
    assert!(
        matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
        "unauthenticated admin role grant should be rejected"
    );

    // (2) Authenticated as a non-admin → 403 forbidden, no role written.
    let req = test::TestRequest::post()
        .uri(&format!("/admin/users/{target}/role"))
        .header("authorization", actor.bearer())
        .set_json(&serde_json::json!({"role": "admin"}))
        .to_request();
    let status = test::call_service(&app, req).await.status();

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(count_role(&fx.state.control_pg, target, "admin").await, 0);

    actor.cleanup(&fx.state).await;
    cleanup_user(&fx.state.control_pg, target).await;

    // Teardown: the service and the fixture both hold connections, and locals
    // are dropped only after the body returns - by which point the runtime is
    // gone and the sockets can no longer be closed. Drop them explicitly, then
    // wait for the close to land.
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn admin_can_grant_platform_role() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "grant").await;
    let pat = common::authz_fixture::admin_principal(&fx.state).await;
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
    let status = test::call_service(&app, req).await.status();

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(count_role(&fx.state.control_pg, target, "support").await, 1);
    assert_eq!(
        count_audit(&fx.state.control_pg, "platform_role_granted", target).await,
        1
    );

    cleanup_user(&fx.state.control_pg, target).await;
    pat.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn admin_can_revoke_platform_role() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "revoke").await;
    let pat = common::authz_fixture::admin_principal(&fx.state).await;
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
    let status = test::call_service(&app, req).await.status();

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(count_role(&fx.state.control_pg, target, "support").await, 0);

    cleanup_user(&fx.state.control_pg, target).await;
    pat.cleanup(&fx.state).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}


