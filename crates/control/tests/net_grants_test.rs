//! Live-PG tests for creator self-service egress grants
//! (`/api/apps/{id}/net-grants`).
//!
//! These drive the REAL ntex handlers with a real `AuthzGuard` bearer, so the
//! authorization they assert is the Cedar `env:read`/`env:write` on
//! `Resource::App` decision, not a stub. What they pin:
//!
//! - an app OWNER can grant, list and revoke on their own app;
//! - a creator who is not a member of the app is refused on all three, and no
//!   row appears;
//! - a host the suffix catalog or the compiled-in backstop forbids is refused,
//!   with the plan caps unchanged;
//! - the plan's `max_grants` ceiling refuses the grant past it.
//!
//! What they do NOT cover: that the rows reach the data plane. That is
//! `plan_catalog.rs::get_versions_projects_app_net_grants_with_plan_caps`,
//! which drives the same store functions and then reads `get_versions`.

use std::path::PathBuf;
use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::plan_catalog::{Plan, PlanCatalog};
use zeroship_control::pricing::{PlanPrice, FX_SCALE};
use zeroship_control::{
    net_grants, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppNetPolicyLimits, AppRuntimeLimits};

mod common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> String {
    common::require_control_db()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("zship-net-grants-{label}-{}", Uuid::new_v4().simple()));
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
    let (control_pg_client, control_pg_conn) =
        connect(db_url, NoTls).await.expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::plan_catalog::seed_plans(&registry)
        .await
        .expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
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
        auth_provider: zeroship_control::platform_auth_provider(
            "https://auth.zeroship.test/oauth2",
            Some(common::platform_jwks_url()),
        ),
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

/// Seed a plan whose only interesting field is its `max_grants` ceiling.
async fn seed_plan_with_max_grants(catalog: &PlanCatalog, max_grants: u32) -> Plan {
    let plan = Plan {
        id: zeroship_core::typed_id::new_plan_id(),
        name: format!("net-grants-cap-{max_grants}"),
        price: PlanPrice {
            base_fee_cents: 0,
            included_units: 100_000,
            fx_pico_cents_per_unit: Some(FX_SCALE as u64),
            spend_limit_default_cents: 0,
        },
        runtime: AppRuntimeLimits {
            cpu_limit_ms: Some(30_000),
            wall_timeout_ms: Some(30_000),
            heap_limit_mb: Some(256),
        },
        net: AppNetPolicyLimits {
            max_sockets: 7,
            egress_ceiling_bytes: 11 * 1024 * 1024,
            max_grants,
        },
        archived: false,
        assignable_by_creator: false,
    };
    catalog.upsert(&plan, Some(plan.archived)).await.expect("upsert plan")
}

async fn count_grants(pg: &Client, app_id: Uuid) -> i64 {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_net_grants WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("count grants");
    rows[0].get("n")
}

async fn cleanup_app(pg: &Client, app_id: Uuid) {
    let _ = pg
        .execute("DELETE FROM zeroship.app_net_grants WHERE app_id = $1", &[&app_id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.app_audit WHERE app_id = $1", &[&app_id])
        .await;
}

#[compio::test]
async fn owner_can_grant_list_and_revoke_their_own_app() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "owner").await;
    let owner = common::authz_fixture::non_admin_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("net-owner-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &owner.user_id,
        )
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(net_grants::configure),
    )
    .await;

    // Deny-by-default: a fresh app holds no grants at all.
    let req = test::TestRequest::get()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", owner.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    assert_eq!(body["grants"].as_array().unwrap().len(), 0);
    assert_eq!(body["limits"]["used_grants"], 0);
    assert_eq!(body["limits"]["max_grants"], 10, "free tier ceiling");

    // Grant. The host is normalized on the way in.
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", owner.bearer())
        .set_json(&serde_json::json!({
            "host": "SMTP.Example.COM.",
            "port": 587,
            "note": "mail relay"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("grant json");
    assert_eq!(body["host"], "smtp.example.com");
    assert_eq!(body["port"], 587);
    assert_eq!(
        body["granted_by"],
        owner.user_id.to_string(),
        "the creator is recorded as the author, not an operator"
    );

    let req = test::TestRequest::get()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", owner.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    assert_eq!(body["grants"].as_array().unwrap().len(), 1);
    assert_eq!(body["limits"]["used_grants"], 1);

    let req = test::TestRequest::delete()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", owner.bearer())
        .set_json(&serde_json::json!({"host": "smtp.example.com", "port": 587}))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(count_grants(&fx.state.control_pg, app_record.id).await, 0);

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn a_creator_cannot_touch_another_creators_app() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "stranger").await;
    let owner = common::authz_fixture::non_admin_principal(&fx.state).await;
    let stranger = common::authz_fixture::non_admin_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("net-stranger-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &owner.user_id,
        )
        .await
        .expect("create app");

    // The owner grants one host, so the stranger's DELETE has a real row to
    // aim at: a 403 on an EMPTY table would not distinguish refusal from
    // "nothing to delete".
    net_grants::upsert_grant(
        fx.state.control_pg.as_ref(),
        app_record.id,
        &net_grants::NetGrantBody {
            host: "smtp.example.com".to_string(),
            port: 587,
            note: None,
        },
        &owner.user_id.to_string(),
    )
    .await
    .expect("owner seed grant");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(net_grants::configure),
    )
    .await;

    let read = test::TestRequest::get()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", stranger.bearer())
        .to_request();
    assert_eq!(
        test::call_service(&app, read).await.status(),
        StatusCode::FORBIDDEN,
        "a non-member must not read another creator's egress hosts"
    );

    let write = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", stranger.bearer())
        .set_json(&serde_json::json!({"host": "evil.example.com", "port": 443}))
        .to_request();
    assert_eq!(
        test::call_service(&app, write).await.status(),
        StatusCode::FORBIDDEN
    );

    let revoke = test::TestRequest::delete()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", stranger.bearer())
        .set_json(&serde_json::json!({"host": "smtp.example.com", "port": 587}))
        .to_request();
    assert_eq!(
        test::call_service(&app, revoke).await.status(),
        StatusCode::FORBIDDEN
    );

    assert_eq!(
        count_grants(&fx.state.control_pg, app_record.id).await,
        1,
        "the stranger neither added nor removed a row"
    );

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    stranger.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn forbidden_hosts_are_refused_and_leave_the_plan_caps_alone() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "forbidden").await;
    let owner = common::authz_fixture::non_admin_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("net-forbidden-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &owner.user_id,
        )
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(net_grants::configure),
    )
    .await;

    // `*` and `*.com` are shape refusals; `*.workers.dev` is the compiled-in
    // frontable backstop; `*.co.uk` is the registry-level suffix rule. All four
    // are 400 and none writes a row.
    for host in ["*", "*.com", "*.workers.dev", "*.co.uk"] {
        let req = test::TestRequest::post()
            .uri(&format!("/api/apps/{}/net-grants", app_record.id))
            .header("authorization", owner.bearer())
            .set_json(&serde_json::json!({"host": host, "port": 443}))
            .to_request();
        let status = test::call_service(&app, req).await.status();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{host} must be refused");
    }
    assert_eq!(
        count_grants(&fx.state.control_pg, app_record.id).await,
        0,
        "a refused host writes no row"
    );

    // An exact host under a frontable suffix is still legal: the catalog bounds
    // wildcards, not named destinations.
    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", owner.bearer())
        .set_json(&serde_json::json!({"host": "api.workers.dev", "port": 443}))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);

    // The caps a refused grant leaves behind are the plan's, untouched.
    let req = test::TestRequest::get()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", owner.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    assert_eq!(body["limits"]["max_grants"], 10);
    assert_eq!(body["limits"]["max_sockets"], 4);
    assert_eq!(body["limits"]["egress_ceiling_bytes"], 10 * 1024 * 1024);
    assert_eq!(body["limits"]["used_grants"], 1);

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn a_grant_past_the_plan_cap_is_refused() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "cap").await;
    let catalog = PlanCatalog::new(fx.state.registry.clone());
    let plan = seed_plan_with_max_grants(&catalog, 1).await;
    let owner = common::authz_fixture::non_admin_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("net-cap-{}", Uuid::new_v4().simple()),
            &plan.id,
            &owner.user_id,
        )
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(net_grants::configure),
    )
    .await;

    let first = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", owner.bearer())
        .set_json(&serde_json::json!({"host": "one.example.com", "port": 5432}))
        .to_request();
    assert_eq!(test::call_service(&app, first).await.status(), StatusCode::OK);

    let second = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", owner.bearer())
        .set_json(&serde_json::json!({"host": "two.example.com", "port": 5432}))
        .to_request();
    let resp = test::call_service(&app, second).await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "the grant past max_grants is refused"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("cap json");
    assert_eq!(body["max_grants"], 1);
    assert_eq!(body["used_grants"], 1);

    // Re-noting the host already granted is not a new row, so the cap does not
    // block it. Without this the ceiling would freeze an app's existing hosts.
    let renote = test::TestRequest::post()
        .uri(&format!("/api/apps/{}/net-grants", app_record.id))
        .header("authorization", owner.bearer())
        .set_json(&serde_json::json!({
            "host": "one.example.com",
            "port": 5432,
            "note": "primary replica"
        }))
        .to_request();
    let resp = test::call_service(&app, renote).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("renote json");
    assert_eq!(body["note"], "primary replica");

    assert_eq!(count_grants(&fx.state.control_pg, app_record.id).await, 1);

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    drop(app);
    drop(catalog);
    drop(fx);
    common::drain_pg().await;
}
