//! MAJOR-4 regression: creator self-assignment of a plan is GUARDED.
//!
//! Drives the REAL `api::set_plan` HTTP handler through ntex (AuthzGuard +
//! two-call enforce) against a live, migrated Postgres:
//!
//!   * an app_owner (creator) PAT may assign ONLY a plan flagged
//!     `assignable_by_creator = true` — a `false` plan is 403;
//!   * an operator PAT (BillingWrite on Resource::Any, here a platform admin)
//!     may assign EITHER.
//!
//! Pre-fix `set_plan` was gated only by BillingWrite/Resource::App (which an
//! app_owner satisfies) + an existence/archive check — so a creator could
//! self-assign a cheaper operator plan and underpay.
//!
//! Set `CONTROL_TEST_DB` to run; silently skips otherwise.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use compio_postgres::{connect, Client, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_authz::{policy_hash, Action, Effect, Policy, Resource, Statement};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::plan_catalog::{Plan, PlanCatalog};
use zeroship_control::pricing::{PlanPrice, FX_SCALE};
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};
use zeroship_core::types::{AppNetPolicyLimits, AppRuntimeLimits};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
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
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
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
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
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

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
    }
}

/// A PAT, plus the user_id it is bound to, for one principal.
struct Pat {
    user_id: Uuid,
    token_id: Uuid,
    token: String,
}

impl Pat {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

/// Issue a PAT for `user_id` with the given wrapper `policy`. Optionally grant a
/// `platform_admin_roles` role (for the operator path).
async fn issue_pat(state: &AppState, user_id: Uuid, role: Option<&str>, policy: Policy) -> Pat {
    if let Some(role) = role {
        state
            .control_pg
            .execute(
                "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
                 VALUES ($1, $2, $1) ON CONFLICT DO NOTHING",
                &[&user_id, &role],
            )
            .await
            .expect("insert platform role");
    }
    let token_id = Uuid::new_v4();
    let policies = policy.to_json_value();
    let hash = policy_hash(&policies);
    let expires_at = Utc::now() + Duration::days(1);
    let token = state
        .pat_issuer
        .issue(token_id, user_id, hash.clone(), expires_at)
        .expect("issue PAT");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.permission_tokens \
                (id, owner_id, kind, name, policies, policy_hash, expires_at) \
             VALUES ($1, $2, 'pat', 'setplan PAT', $3, $4, $5)",
            &[&token_id, &user_id, &policies, &hash, &expires_at],
        )
        .await
        .expect("insert PAT row");
    Pat { user_id, token_id, token }
}

/// Wrapper policy granting BillingWrite on a specific app (the app_owner upper
/// bound: a creator's token may act on their own app).
fn billing_write_on_app(app_id: Uuid) -> Policy {
    Policy {
        name: "creator billing".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::BillingWrite],
            resources: vec![Resource::App { id: app_id.to_string() }],
            conditions: Vec::new(),
        }],
    }
}

/// Wrapper policy granting BillingWrite fleet-wide (the operator upper bound).
fn billing_write_any() -> Policy {
    Policy {
        name: "operator billing".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::BillingWrite],
            resources: vec![Resource::Any],
            conditions: Vec::new(),
        }],
    }
}

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
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
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
        .create_app(&format!("setplan-{}", Uuid::new_v4().simple()), &start.id, &owner)
        .await
        .expect("create app");

    let creator_pat = issue_pat(&fx.state, owner, None, billing_write_on_app(app.id)).await;

    // An operator: platform 'billing' role + BillingWrite on Resource::Any.
    let op_user = make_user(&pg, "operator").await;
    let operator_pat = issue_pat(&fx.state, op_user, Some("billing"), billing_write_any()).await;

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
    let resp = test::call_service(
        &app_svc,
        put(creator_pat.bearer(), app.id, operator_only.id.clone()),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "creator must NOT self-assign a non-assignable (operator) plan",
    );
    assert_eq!(
        app_plan_id(&pg, app.id).await,
        start.id,
        "a rejected creator assignment must not change the plan",
    );

    // 2. Creator assigning an ASSIGNABLE plan ⇒ 200, plan updated.
    let resp = test::call_service(
        &app_svc,
        put(creator_pat.bearer(), app.id, assignable.id.clone()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "creator may assign an assignable plan");
    assert_eq!(app_plan_id(&pg, app.id).await, assignable.id, "creator assignment applied");

    // 3. Operator assigning the OPERATOR-ONLY plan ⇒ 200 (operator may assign
    //    EITHER), plan updated.
    let resp = test::call_service(
        &app_svc,
        put(operator_pat.bearer(), app.id, operator_only.id.clone()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "operator may assign ANY plan");
    assert_eq!(
        app_plan_id(&pg, app.id).await,
        operator_only.id,
        "operator assignment of the operator-only plan applied",
    );

    // Cleanup (best-effort; FK order: spend state, members, app, tokens, roles).
    let _ = pg.execute("DELETE FROM zeroship.app_spend_state WHERE app_id = $1", &[&app.id]).await;
    let _ = pg.execute("DELETE FROM zeroship.app_members WHERE app_id = $1", &[&app.id]).await;
    let _ = pg.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app.id]).await;
    for pat in [&creator_pat, &operator_pat] {
        let _ = pg
            .execute("DELETE FROM zeroship.authz_decisions WHERE token_id = $1", &[&pat.token_id])
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.permission_tokens WHERE id = $1", &[&pat.token_id])
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&pat.user_id])
            .await;
    }
    let _ = pg.execute("DELETE FROM zeroship.users WHERE id = ANY($1)", &[&vec![owner, op_user]]).await;
}
