//! Integration tests for the operator GLOBAL-FX runtime API (gap #28):
//! `GET`/`PUT /api/pricing-config`.
//!
//! FAITHFUL by construction: the tests drive the REAL ntex HTTP handlers
//! (`get_pricing_config`/`set_pricing_config`) through a real `AuthzGuard` (a
//! real PAT) against a live, migrated Postgres. The handler resolves the global
//! FX through the REAL `PricingStore` against `zeroship.pricing_config`, and the
//! audit row is read back from `zeroship.app_audit`. No stubbed authz, no shim.
//!
//! Requires `CONTROL_TEST_DB`; silent skip otherwise. The DB must have changeset
//! 0041 (pricing_config + the FX floor CHECK) applied.

#![allow(clippy::future_not_send)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_authz::{policy_hash, Action, Effect, Policy, Resource, Statement};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    api, token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const FLOOR: u64 = zeroship_control::pricing::MIN_FX_PICO_CENTS_PER_UNIT;

/// `pricing_config` is a fleet-wide SINGLETON (`id='global'`), so these tests
/// mutate shared state. Serialize them with a process-wide lock so the suite is
/// correct under the default multi-threaded runner (not just `--test-threads=1`).
/// A poisoned lock (a prior test panicked mid-section) is recovered — we still
/// want the next test to run and restore the seed.
static FX_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_fx() -> std::sync::MutexGuard<'static, ()> {
    FX_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-pricecfg-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ---------------------------------------------------------------------------
// Fixture: a real AppState over a live PG.
// ---------------------------------------------------------------------------

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

async fn build_fixture(db_url: &str, label: &str) -> Fixture {
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));

    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new("sk_test_mock".to_string()),
        stripe_base_url: "http://127.0.0.1:9".to_string(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: false,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg: Arc::new(control_pg_client),
        hydra_admin_url: "http://127.0.0.1:9".to_string(),
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
        pairwise_salt: [0u8; 32],
    });

    Fixture { state, blob_root, deploy_tmp_dir }
}

// ---------------------------------------------------------------------------
// PAT + principal helpers (faithful AuthzGuard).
// ---------------------------------------------------------------------------

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

async fn make_user(state: &AppState, label: &str) -> Uuid {
    let id = Uuid::now_v7();
    let email = format!("{label}-{}@zeroship.test", id.simple());
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2, $3, NOW())",
            &[&id, &email, &label.to_string()],
        )
        .await
        .expect("insert user");
    id
}

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
             VALUES ($1, $2, 'pat', 'pricing-config PAT', $3, $4, $5)",
            &[&token_id, &user_id, &policies, &hash, &expires_at],
        )
        .await
        .expect("insert PAT row");
    Pat { user_id, token_id, token }
}

/// Operator policy: BillingRead+Write on `Resource::Any` — the fleet-wide gate.
fn billing_any() -> Policy {
    Policy {
        name: "operator".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::BillingRead, Action::BillingWrite],
            resources: vec![Resource::Any],
            conditions: Vec::new(),
        }],
    }
}

/// Self-service policy: BillingRead+Write on the creator's OWN app surface — the
/// creator upper bound. Crucially does NOT grant `Resource::Any` (operator-only),
/// so the `Resource::Any` gate on the pricing-config routes denies it.
fn billing_self() -> Policy {
    Policy {
        name: "creator self".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::BillingRead, Action::BillingWrite],
            resources: vec![Resource::App { id: Uuid::new_v4().to_string() }],
            conditions: Vec::new(),
        }],
    }
}

async fn cleanup(state: &AppState, pats: &[&Pat]) {
    let pg = &state.control_pg;
    for p in pats {
        let _ = pg
            .execute(
                "DELETE FROM zeroship.app_audit WHERE actor_user_id = $1",
                &[&p.user_id],
            )
            .await;
        let _ = pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE token_id = $1 OR actor_user_id = $2",
                &[&p.token_id, &p.user_id],
            )
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.permission_tokens WHERE id = $1", &[&p.token_id])
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&p.user_id])
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&p.user_id])
            .await;
    }
}

/// Restore the seed FX so a test never leaves the singleton repriced for the
/// next test (the table is a fleet-wide singleton shared across the suite).
async fn restore_seed_fx(state: &AppState, fx: u64) {
    let fx_i64 = i64::try_from(fx).expect("seed fx fits i64");
    state
        .control_pg
        .execute(
            "UPDATE zeroship.pricing_config SET fx_pico_cents_per_unit = $1 WHERE id = 'global'",
            &[&fx_i64],
        )
        .await
        .expect("restore seed fx");
}

async fn current_fx(state: &AppState) -> u64 {
    let rows = state
        .control_pg
        .query(
            "SELECT fx_pico_cents_per_unit FROM zeroship.pricing_config WHERE id = 'global'",
            &[],
        )
        .await
        .expect("read fx");
    let v: i64 = rows.first().expect("global row exists").get("fx_pico_cents_per_unit");
    v as u64
}

// ===========================================================================
// Tests
// ===========================================================================

/// PUT as an operator updates the global FX; GET reflects it; an audit row with
/// the actor + old→new is written.
#[compio::test]
async fn operator_put_updates_and_get_reflects_with_audit() {
    let Some(db) = db_url() else { return };
    let _guard = lock_fx();
    let fx = build_fixture(&db, "op-put").await;
    let seed = current_fx(&fx.state).await;

    let op = make_user(&fx.state, "operator").await;
    let op_pat = issue_pat(&fx.state, op, Some("billing"), billing_any()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/pricing-config")
                .route(web::get().to(api::get_pricing_config))
                .route(web::put().to(api::set_pricing_config)),
        ),
    )
    .await;

    let new_fx: u64 = seed + 123_456;

    // PUT as operator → 200.
    let put = test::TestRequest::put()
        .uri("/api/pricing-config")
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({ "fx_pico_cents_per_unit": new_fx }))
        .to_request();
    let resp = test::call_service(&svc, put).await;
    assert_eq!(resp.status(), StatusCode::OK, "operator PUT should succeed");

    // The stored value changed.
    assert_eq!(current_fx(&fx.state).await, new_fx, "DB reflects the new FX");

    // GET reflects it.
    let get = test::TestRequest::get()
        .uri("/api/pricing-config")
        .header("authorization", op_pat.bearer())
        .to_request();
    let body: serde_json::Value = test::read_response_json(&svc, get).await;
    assert_eq!(
        body["fx_pico_cents_per_unit"].as_u64(),
        Some(new_fx),
        "GET returns the new FX"
    );

    // An audit row with the actor + old→new transition.
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT actor_user_id, detail FROM zeroship.app_audit \
             WHERE action = 'set_global_fx' AND actor_user_id = $1",
            &[&op],
        )
        .await
        .expect("read audit");
    assert_eq!(rows.len(), 1, "exactly one set_global_fx audit row");
    let actor: Option<Uuid> = rows[0].get("actor_user_id");
    assert_eq!(actor, Some(op), "audit records the operator as actor");
    let detail: serde_json::Value = rows[0].get("detail");
    assert_eq!(
        detail["old_fx_pico_cents_per_unit"].as_u64(),
        Some(seed),
        "audit records the OLD fx"
    );
    assert_eq!(
        detail["new_fx_pico_cents_per_unit"].as_u64(),
        Some(new_fx),
        "audit records the NEW fx"
    );

    restore_seed_fx(&fx.state, seed).await;
    cleanup(&fx.state, &[&op_pat]).await;
}

/// PUT as a creator (app-scoped principal, NOT Resource::Any) → 403, and the
/// stored value is unchanged. This is the operator-only invariant.
#[compio::test]
async fn creator_put_is_forbidden_and_value_unchanged() {
    let Some(db) = db_url() else { return };
    let _guard = lock_fx();
    let fx = build_fixture(&db, "creator-403").await;
    let seed = current_fx(&fx.state).await;

    let creator = make_user(&fx.state, "creator").await;
    // No platform role; only an app-scoped BillingWrite grant.
    let creator_pat = issue_pat(&fx.state, creator, None, billing_self()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/pricing-config")
                .route(web::get().to(api::get_pricing_config))
                .route(web::put().to(api::set_pricing_config)),
        ),
    )
    .await;

    let put = test::TestRequest::put()
        .uri("/api/pricing-config")
        .header("authorization", creator_pat.bearer())
        .set_json(&serde_json::json!({ "fx_pico_cents_per_unit": seed + 999 }))
        .to_request();
    let resp = test::call_service(&svc, put).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "creator (app-scoped, not Resource::Any) must be denied — operator-only"
    );

    // The stored value is untouched.
    assert_eq!(current_fx(&fx.state).await, seed, "creator PUT must not change FX");

    // GET as a creator is also operator-only → 403.
    let get = test::TestRequest::get()
        .uri("/api/pricing-config")
        .header("authorization", creator_pat.bearer())
        .to_request();
    let resp = test::call_service(&svc, get).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "creator GET must be denied — operator-only read"
    );

    cleanup(&fx.state, &[&creator_pat]).await;
}

/// PUT a below-floor value → 400, and the stored value is unchanged (fail
/// closed — the floor is enforced at the handler, not just the DB CHECK).
#[compio::test]
async fn below_floor_put_is_rejected_400_value_unchanged() {
    let Some(db) = db_url() else { return };
    let _guard = lock_fx();
    let fx = build_fixture(&db, "below-floor").await;
    let seed = current_fx(&fx.state).await;

    let op = make_user(&fx.state, "operator").await;
    let op_pat = issue_pat(&fx.state, op, Some("billing"), billing_any()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/pricing-config")
                .route(web::get().to(api::get_pricing_config))
                .route(web::put().to(api::set_pricing_config)),
        ),
    )
    .await;

    assert!(FLOOR >= 1, "floor sanity");
    let below = FLOOR - 1;

    let put = test::TestRequest::put()
        .uri("/api/pricing-config")
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({ "fx_pico_cents_per_unit": below }))
        .to_request();
    let resp = test::call_service(&svc, put).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "below-floor FX must be rejected with 400 (fail closed)"
    );

    // The stored value is untouched, and NO audit row was written.
    assert_eq!(current_fx(&fx.state).await, seed, "below-floor PUT must not change FX");
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT 1 FROM zeroship.app_audit WHERE action = 'set_global_fx' AND actor_user_id = $1",
            &[&op],
        )
        .await
        .expect("read audit");
    assert!(rows.is_empty(), "a rejected below-floor PUT must not audit a write");

    // The exact floor IS accepted (boundary is inclusive).
    let put_floor = test::TestRequest::put()
        .uri("/api/pricing-config")
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({ "fx_pico_cents_per_unit": FLOOR }))
        .to_request();
    let resp = test::call_service(&svc, put_floor).await;
    assert_eq!(resp.status(), StatusCode::OK, "the exact floor is accepted");
    assert_eq!(current_fx(&fx.state).await, FLOOR, "floor value stored");

    restore_seed_fx(&fx.state, seed).await;
    cleanup(&fx.state, &[&op_pat]).await;
}
