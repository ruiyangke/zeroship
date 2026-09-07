//! MAJOR-4 regression: creator self-assignment of a plan is GUARDED.
//!
//! Drives the REAL `api::set_plan` HTTP handler through ntex (AuthzGuard +
//! two-call enforce) against a live, migrated Postgres:
//!
//!   * an app_owner (creator) bearer may assign ONLY a plan flagged
//!     `assignable_by_creator = true` — a `false` plan is 403;
//!   * an operator bearer (BillingWrite on Resource::Any, here a platform admin)
//!     may assign EITHER.
//!
//! Pre-fix `set_plan` was gated only by BillingWrite/Resource::App (which an
//! app_owner satisfies) + an existence/archive check — so a creator could
//! self-assign a cheaper operator plan and underpay.
//!
//! Configure a test database (`zeroship_core::config::test_database_url_opt`;
//! run `tests/provision_test_backends.sh` to provision one) to run; silently
//! skips otherwise.

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
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};
use zeroship_core::types::{AppNetPolicyLimits, AppRuntimeLimits};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> String {
    crate::common::require_control_db()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("zship-setplan-{label}-{}", Uuid::new_v4().simple()));
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
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );

    let state = Arc::new(AppState {
        service_auth: std::sync::Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured()),
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
        worker_urls: Vec::new(),
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

/// A bearer, plus the user_id it is bound to, for one principal.
struct Caller {
    user_id: Uuid,
    token: String,
}

impl Caller {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

/// Issue a platform OAuth bearer for `user_id` carrying `scope`. Optionally
/// grant a `platform_admin_roles` role (for the operator path).
/// Issue a platform OAuth bearer for `user_id` carrying `scope`.
///
/// It used to take an optional platform role and seed a `platform_admin_roles`
/// row for the operator paths. That table and those roles are deleted, so every
/// principal this mints is an ordinary creator.
async fn issue_bearer(state: &AppState, user_id: Uuid, scope: &str) -> Caller {
    let _ = state;
    Caller {
        user_id,
        token: common::platform_token_for_client(user_id, scope, common::CONSOLE_CLIENT_ID),
    }
}

// The scope vocabulary is resource-blind: a scope always lowers to
// `Resource::Any`, so the former `billing_write_on_app`/`billing_write_any`
// per-resource wrapper policies collapse to the SAME scope string,
// "billing:write". The app_owner-vs-operator distinction below is carried
// entirely by the static Cedar policy: the app_owner path is allowed because
// the creator's real `app_members` ownership row grants BillingWrite on their
// OWN app (`set_plan`'s per-app gate), while the operator path additionally
// needs a `platform_admin_roles` row (role "billing") for the fleet-wide
// `Resource::Any` grant.

/// Seed a plan with a given `assignable_by_creator` flag. Mints a fresh id.
async fn seed_plan(catalog: &PlanCatalog, name: &str, assignable: bool) -> Plan {
    let plan = Plan {
        id: zeroship_core::typed_id::new_plan_id(),
        name: name.to_string(),
        price: PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(FX_SCALE as u64),
            spend_limit_default_cents: 0,
        },
        runtime: AppRuntimeLimits {
            cpu_limit_ms: Some(30_000),
            wall_timeout_ms: Some(30_000),
            heap_limit_mb: Some(256),
        },
        net: AppNetPolicyLimits {
            max_sockets: 32,
            egress_ceiling_bytes: 256 * 1024 * 1024,
            max_grants: 8,
        },
        archived: false,
        assignable_by_creator: assignable,
    };
    catalog.upsert(&plan, Some(false)).await.expect("seed plan")
}

async fn make_user(pg: &Client, label: &str) -> Uuid {
    let id = Uuid::now_v7();
    let email = format!("{label}-{}@zeroship.test", id.simple());
    pg.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2, $3, NOW())",
        &[&id, &email, &label],
    )
    .await
    .expect("insert user");
    id
}

async fn app_plan_id(pg: &Client, app_id: Uuid) -> String {
    pg.query_one("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await
        .expect("read app plan")
        .get("plan_id")
}

#[compio::test]
async fn creator_cannot_self_assign_non_assignable_plan_operator_can() {
    let url = db_url();
    let fx = build_test_state(&url, "major4").await;
    let pg = fx.state.control_pg.clone();
    let catalog = PlanCatalog::new(fx.state.registry.clone());

    // Two plans: one assignable by a creator, one operator-only.
    let assignable = seed_plan(&catalog, "creator-tier", true).await;
    let operator_only = seed_plan(&catalog, "operator-tier", false).await;
    // A starting plan the app sits on (assignable, so the creator could have
    // landed there legitimately).
    let start = seed_plan(&catalog, "start-tier", true).await;

    // The creator owns an app (create_app writes the owner app_members row).
    let owner = make_user(&pg, "creator").await;
    let app = fx
        .state
        .registry
        .create_app(&format!("setplan-{}", Uuid::new_v4().simple()), &start.id, &owner, None)
        .await
        .expect("create app");

    let creator_caller = issue_bearer(&fx.state, owner, "billing:write").await;

    // An operator: platform 'billing' role + BillingWrite on Resource::Any.
    let op_user = make_user(&pg, "operator").await;
    let operator_caller = issue_bearer(&fx.state, op_user, "billing:write").await;

    let app_svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/apps/{id}/plan").route(web::put().to(api::set_plan)),
        ),
    )
    .await;

    let put = |bearer: String, app_id: Uuid, plan_id: String| {
        test::TestRequest::put()
            .uri(&format!("/api/apps/{app_id}/plan"))
            .header("authorization", bearer)
            .set_json(&serde_json::json!({ "plan_id": plan_id }))
            .to_request()
    };

    // 1. Creator assigning the OPERATOR-ONLY plan ⇒ 403, plan unchanged.
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown at the end of this test.
    let status = test::call_service(
        &app_svc,
        put(creator_caller.bearer(), app.id, operator_only.id.clone()),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "creator must NOT self-assign a non-assignable (operator) plan",
    );
    assert_eq!(
        app_plan_id(&pg, app.id).await,
        start.id,
        "a rejected creator assignment must not change the plan",
    );

    // 2. Creator assigning an ASSIGNABLE plan ⇒ 200, plan updated.
    let status = test::call_service(
        &app_svc,
        put(creator_caller.bearer(), app.id, assignable.id.clone()),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::OK, "creator may assign an assignable plan");
    assert_eq!(app_plan_id(&pg, app.id).await, assignable.id, "creator assignment applied");

    // 3. A SECOND creator who owns nothing here is refused outright. There is
    //    no operator arm to assign the operator-only plan any more: it was
    //    satisfiable only by the deleted universal-allow policy, so a
    //    non-creator-assignable tier is now set by editing the catalog row, not
    //    through this endpoint.
    let status = test::call_service(
        &app_svc,
        put(operator_caller.bearer(), app.id, operator_only.id.clone()),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a caller with no membership of the app may not assign its plan",
    );
    assert_eq!(
        app_plan_id(&pg, app.id).await,
        assignable.id,
        "the refused assignment leaves the creator's plan in place",
    );

    // Cleanup (best-effort; FK order: spend state, members, app, tokens, roles).
    let _ = pg.execute("DELETE FROM zeroship.app_spend_state WHERE app_id = $1", &[&app.id]).await;
    let _ = pg.execute("DELETE FROM zeroship.organization_members om \
                 USING zeroship.apps a JOIN zeroship.projects p ON p.id = a.project_id \
                 WHERE om.organization_id = p.organization_id AND a.id = $1", &[&app.id]).await;
    let _ = pg.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app.id]).await;
    for caller in [&creator_caller, &operator_caller] {
        let _ = pg
            .execute("DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1", &[&caller.user_id])
            .await;
    }
    let _ = pg.execute("DELETE FROM zeroship.users WHERE id = ANY($1)", &[&vec![owner]]).await;

    // Teardown: the service, the plan catalog, the cloned `pg` handle, and the
    // fixture all hold (or share) a Postgres connection, and locals are dropped
    // only after the body returns - by which point the runtime is gone and the
    // sockets can no longer be closed. Drop them explicitly, then wait for the
    // close to land.
    drop(app_svc);
    drop(catalog);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}

/// An ARCHIVED plan must be refused with a reason, not reported as a missing app.
///
/// Lives in the authz file because this is the only harness that drives the real
/// `api::set_plan` through ntex; the property itself is plan validity, not authz.
///
/// `api.rs` says above the catalog lookup that "The target plan must be a real,
/// non-archived plan; validate it via the catalog", but the arm is
/// `Ok(Some(_)) => {}` and `PlanCatalog::get` selects `archived` WITHOUT
/// filtering on it. The proration UPDATE downstream is guarded on
/// `EXISTS (... AND NOT archived)`, so an archived plan matches zero rows and
/// becomes `PlanChangeOutcome::AppNotFound` -> 404 "app not found", for an app
/// that plainly exists.
///
/// Uses the app OWNER, and the archived plan is seeded `assignable_by_creator`
/// deliberately: the caller must get PAST the assignability gate so the archived
/// check is what answers, rather than a 403 hiding the defect. This used to
/// reach that point with an operator token, which no principal can hold now.
#[compio::test]
async fn assigning_an_archived_plan_is_refused_and_not_reported_as_a_missing_app() {
    let url = db_url();
    let fx = build_test_state(&url, "archived-plan").await;
    let pg = fx.state.control_pg.clone();
    let catalog = PlanCatalog::new(fx.state.registry.clone());

    let start = seed_plan(&catalog, "start-tier", true).await;
    // Seed assignable, then archive it. `upsert`'s second arg is `archived`.
    let retired = seed_plan(&catalog, "retired-tier", true).await;
    catalog.upsert(&retired, Some(true)).await.expect("archive the plan");

    let owner = make_user(&pg, "creator").await;
    let app = fx
        .state
        .registry
        .create_app(&format!("setplan-arch-{}", Uuid::new_v4().simple()), &start.id, &owner, None)
        .await
        .expect("create app");

    let owner_caller = issue_bearer(&fx.state, owner, "billing:write").await;

    let app_svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/apps/{id}/plan").route(web::put().to(api::set_plan)),
        ),
    )
    .await;

    let status = test::call_service(
        &app_svc,
        test::TestRequest::put()
            .uri(&format!("/api/apps/{}/plan", app.id))
            .header("authorization", owner_caller.bearer())
            .set_json(&serde_json::json!({ "plan_id": retired.id }))
            .to_request(),
    )
    .await
    .status();

    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "an archived plan must not surface as 404 app-not-found: the app exists, \
         and that status sends the caller looking for a deleted app",
    );
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "assigning an archived plan is a bad request, like an unknown plan id",
    );
    assert_eq!(
        app_plan_id(&pg, app.id).await,
        start.id,
        "a refused assignment must leave the plan untouched",
    );

    let _ = pg.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app.id]).await;
    let _ = pg
        .execute("DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1", &[&owner_caller.user_id])
        .await;
    let _ = pg.execute("DELETE FROM zeroship.users WHERE id = ANY($1)", &[&vec![owner]]).await;

    drop(app_svc);
    drop(catalog);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}
