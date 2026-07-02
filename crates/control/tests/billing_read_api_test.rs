//! PR-7 (billing-ops gap #26) — creator billing READ APIs, FAITHFUL suite.
//!
//! Drives the REAL ntex handlers (`api::list_app_invoices`, `get_invoice`,
//! `get_projected_charge`, `get_credit_balance`, `get_payment_method`,
//! `get_billing_status`) through a REAL `AuthzGuard` (real BillingRead PATs) and
//! the REAL `crate::billing_read` data layer against a live, migrated Postgres.
//! NO shims: the same router wiring, the same Cedar enforce, the same
//! `charge_cents` pricing kernel the reconciler runs.
//!
//! The SECURITY core (a creator must never read another creator's billing):
//!   - a creator reads ONLY their own apps' invoices/lines/credit;
//!   - a SECOND creator's data is invisible (cross-creator read → 403/empty);
//!   - an unauthorized / wrong-app token → 403;
//!   - an operator (`Resource::Any`) reads any creator;
//!   - the invoice line detail returns the FROZEN snapshot that reproduces
//!     `amount_cents` via `charge_cents`;
//!   - the projected-charge response is labelled non-authoritative AND the cache
//!     budget holds (a 2nd call within the TTL does NOT re-price — asserted via
//!     the cache's reprice counter, faithfully);
//!   - the credit balance reflects grants/consumes.
//!
//! PARALLEL-SAFE (the PR-6 lesson): every test mints UNIQUE creators/apps and
//! asserts per-creator scope, so the default cargo runner can run them
//! concurrently without cross-test interference.
//!
//! Set `CONTROL_TEST_DB` to run; silently skips otherwise.

#![allow(clippy::future_not_send)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Duration, Utc};
use compio_postgres::{connect, Client, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_authz::{policy_hash, Action, Effect, Policy, Resource, Statement};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::pricing::{charge_cents, MetricWeight, MetricWeights, PlanPrice, FX_SCALE};
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("zship-billread-{label}-{}", Uuid::new_v4().simple()));
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
        projected_charge_cache: Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
    }
}

// --------------------------------------------------------------------------
// PAT minting + policy helpers
// --------------------------------------------------------------------------

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
             VALUES ($1, $2, 'pat', 'billread PAT', $3, $4, $5)",
            &[&token_id, &user_id, &policies, &hash, &expires_at],
        )
        .await
        .expect("insert PAT row");
    Pat {
        user_id,
        token_id,
        token,
    }
}

/// BillingRead on a specific app — the app-owner (creator) upper bound.
fn billing_read_on_app(app_id: Uuid) -> Policy {
    Policy {
        name: "creator billing read".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::BillingRead],
            resources: vec![Resource::App { id: app_id.to_string() }],
            conditions: Vec::new(),
        }],
    }
}

/// BillingRead fleet-wide — the operator upper bound.
fn billing_read_any() -> Policy {
    Policy {
        name: "operator billing read".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::BillingRead],
            resources: vec![Resource::Any],
            conditions: Vec::new(),
        }],
    }
}

/// A token with NO billing capability at all (empty policy) — the unauthorized
/// caller used to assert a 403 on the billing reads.
fn empty_policy() -> Policy {
    Policy {
        name: "no-grants".to_owned(),
        statements: Vec::new(),
    }
}

// --------------------------------------------------------------------------
// Seeding helpers (faithful: real schema, real pricing inputs)
// --------------------------------------------------------------------------

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

/// Seed a plan row directly (base fee, included units, explicit FX). Returns id.
async fn seed_plan(
    pg: &Client,
    base_fee_cents: i64,
    included_units: i64,
    fx_pico: i64,
    spend_default: i64,
) -> String {
    let id = zeroship_core::typed_id::new_plan_id();
    pg.execute(
        "INSERT INTO zeroship.plans \
           (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
            runtime_limits_json, spend_limit_default_cents) \
         VALUES ($1, 'read-tier', $2, $3, $4, \
                 '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', $5)",
        &[&id, &base_fee_cents, &included_units, &fx_pico, &spend_default],
    )
    .await
    .expect("seed plan");
    id
}

/// Create an app owned by `owner` (writes the `app_members(owner)` row).
async fn make_app(registry: &Registry, plan_id: &str, owner: &Uuid) -> Uuid {
    registry
        .create_app(&format!("read-{}", Uuid::new_v4().simple()), plan_id, owner)
        .await
        .expect("create app")
        .id
}

/// Add `user` as a NON-owner member (e.g. `viewer`/`editor`) of `app`. Used to
/// model the cross-creator hole: a victim-creator who is merely a viewer on the
/// attacker's app appears in the attacker's role-AGNOSTIC `list_apps_for_owner`.
async fn add_member(pg: &Client, app: &Uuid, user: &Uuid, role: &str) {
    pg.execute(
        "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, $3) \
         ON CONFLICT DO NOTHING",
        &[app, user, &role],
    )
    .await
    .expect("add non-owner member");
}

/// Ensure a `creator_billing` row exists (the FK target for invoices/credit).
async fn ensure_creator_billing(pg: &Client, creator_id: &Uuid, default_pm_set: bool) {
    pg.execute(
        "INSERT INTO zeroship.creator_billing (creator_id, default_pm_set) \
         VALUES ($1, $2) \
         ON CONFLICT (creator_id) DO UPDATE SET default_pm_set = EXCLUDED.default_pm_set",
        &[creator_id, &default_pm_set],
    )
    .await
    .expect("ensure creator_billing");
}

/// Seed a Stripe customer ref (presence only is surfaced; the raw id stays internal).
async fn seed_customer_ref(pg: &Client, creator_id: &Uuid, external_id: &str) {
    pg.execute(
        "INSERT INTO zeroship.billing_customer_refs (creator_id, provider, external_id) \
         VALUES ($1, 'stripe', $2) ON CONFLICT DO NOTHING",
        &[creator_id, &external_id],
    )
    .await
    .expect("seed customer ref");
}

/// Seed a FINALIZED invoice with ONE frozen segment line that reproduces
/// `amount_cents` via `charge_cents`. Returns `(invoice_id, amount_cents)`.
///
/// FAITHFUL reproducibility: we compute the breakdown with the SAME kernel the
/// reconciler uses, freeze the inputs into the line, then finalize. The line
/// detail test re-runs `charge_cents` over the frozen snapshot and asserts it
/// equals the frozen `amount_cents`.
#[allow(clippy::too_many_arguments)]
async fn seed_finalized_invoice(
    pg: &Client,
    creator_id: &Uuid,
    app_id: &Uuid,
    plan_id: &str,
    period: chrono::NaiveDate, // first-of-month
    price: &PlanPrice,
    usage: &std::collections::HashMap<String, i64>,
    weights: &MetricWeights,
) -> (String, i64) {
    let breakdown = charge_cents(price, usage, weights).expect("price");
    let amount = i64::try_from(breakdown.total_cents).expect("amount fits i64");
    let fx = i64::try_from(price.fx_pico_cents_per_unit.expect("resolved fx")).expect("fx fits");
    let included = i64::try_from(price.included_units).expect("included fits");
    let base = i64::try_from(price.base_fee_cents).expect("base fits");

    let invoice_id = zeroship_core::typed_id::new_invoice_id();
    // 1. draft invoice (lines are mutable while draft).
    pg.execute(
        "INSERT INTO zeroship.invoices \
           (id, creator_id, period, status, currency, subtotal_cents, credit_cents, tax_cents, total_cents) \
         VALUES ($1, $2, $3::date, 'draft', 'usd', $4, 0, 0, $4)",
        &[&invoice_id, creator_id, &period, &amount],
    )
    .await
    .expect("insert draft invoice");

    // 2. the frozen segment line (segment_no 0).
    let usage_json = serde_json::to_value(usage).expect("usage json");
    let weights_json = serde_json::to_value(weights).expect("weights json");
    pg.execute(
        "INSERT INTO zeroship.invoice_lines \
           (invoice_id, app_id, segment_no, plan_id, included_units, fx_pico_cents_per_unit, \
            base_fee_cents, amount_cents, usage_snapshot, weights_snapshot) \
         VALUES ($1, $2, 0, $3, $4, $5, $6, $7, $8, $9)",
        &[
            &invoice_id,
            app_id,
            &plan_id,
            &included,
            &fx,
            &base,
            &amount,
            &usage_json,
            &weights_json,
        ],
    )
    .await
    .expect("insert frozen line");

    // 3. finalize (freezes the line via the immutability trigger).
    pg.execute(
        "UPDATE zeroship.invoices SET status = 'finalized', finalized_at = NOW() WHERE id = $1",
        &[&invoice_id],
    )
    .await
    .expect("finalize invoice");

    (invoice_id, amount)
}

async fn seed_usage(
    pg: &Client,
    app_id: &Uuid,
    period: chrono::NaiveDate,
    metric: &str,
    total: i64,
) {
    pg.execute(
        "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
         VALUES ($1, $2::date, $3, $4) \
         ON CONFLICT (app_id, period, metric) DO UPDATE SET total = EXCLUDED.total",
        &[app_id, &period, &metric, &total],
    )
    .await
    .expect("seed usage");
}

/// Seed a global weight row + the global default FX so live pricing resolves.
async fn ensure_global_pricing(pg: &Client, metric: &str, units_per_op: i64, per_units: i64) {
    pg.execute(
        "INSERT INTO zeroship.billing_metrics (metric, kind, unit) VALUES ($1, 'platform', 'op') \
         ON CONFLICT (metric) DO NOTHING",
        &[&metric],
    )
    .await
    .expect("seed metric catalog");
    pg.execute(
        "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (metric) DO UPDATE SET units_per_op = EXCLUDED.units_per_op, \
                                            per_units = EXCLUDED.per_units",
        &[&metric, &units_per_op, &per_units],
    )
    .await
    .expect("seed weight");
    // Ensure the global pricing_config singleton EXISTS, but do NOT overwrite its
    // value: it is fleet-wide shared state across every billing test binary on the
    // shared :5440 DB, and every plan in this file carries its OWN plan-level
    // `fx_pico_cents_per_unit` (so the effective FX is the plan's, never the
    // global). Overwriting the singleton to `FX_SCALE` here would (a) permanently
    // corrupt the migrated default (30_000_000) for every subsequent
    // reconcile-family test in the run and (b) race concurrent readers in sibling
    // binaries. `ON CONFLICT DO NOTHING` preserves the migrated default.
    pg.execute(
        "INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit) \
         VALUES ('global', 30000000) \
         ON CONFLICT (id) DO NOTHING",
        &[],
    )
    .await
    .expect("seed global fx");
}

fn current_period() -> chrono::NaiveDate {
    use chrono::Datelike;
    let now = Utc::now();
    chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).expect("first-of-month")
}

async fn cleanup(pg: &Client, creator_ids: &[Uuid], app_ids: &[Uuid], pats: &[&Pat]) {
    for app in app_ids {
        let _ = pg.execute("DELETE FROM zeroship.usage_aggregates WHERE app_id = $1", &[app]).await;
        let _ = pg.execute("DELETE FROM zeroship.invoice_lines WHERE app_id = $1", &[app]).await;
        let _ = pg.execute("DELETE FROM zeroship.app_spend_state WHERE app_id = $1", &[app]).await;
        let _ = pg.execute("DELETE FROM zeroship.app_spend_limit WHERE app_id = $1", &[app]).await;
        let _ = pg.execute("DELETE FROM zeroship.app_members WHERE app_id = $1", &[app]).await;
    }
    for creator in creator_ids {
        // Invoices reference lines (RESTRICT) — lines were dropped above by app_id,
        // but drop any remaining by creator before the invoice rows.
        let _ = pg
            .execute(
                "DELETE FROM zeroship.invoice_lines WHERE invoice_id IN \
                 (SELECT id FROM zeroship.invoices WHERE creator_id = $1)",
                &[creator],
            )
            .await;
        let _ = pg.execute("DELETE FROM zeroship.credit_ledger WHERE creator_id = $1", &[creator]).await;
        let _ = pg.execute("DELETE FROM zeroship.invoices WHERE creator_id = $1", &[creator]).await;
        let _ = pg.execute("DELETE FROM zeroship.billing_customer_refs WHERE creator_id = $1", &[creator]).await;
        let _ = pg.execute("DELETE FROM zeroship.creator_billing_status WHERE creator_id = $1", &[creator]).await;
        let _ = pg.execute("DELETE FROM zeroship.creator_billing WHERE creator_id = $1", &[creator]).await;
    }
    for app in app_ids {
        let _ = pg.execute("DELETE FROM zeroship.apps WHERE id = $1", &[app]).await;
    }
    for pat in pats {
        let _ = pg.execute("DELETE FROM zeroship.authz_decisions WHERE token_id = $1", &[&pat.token_id]).await;
        let _ = pg.execute("DELETE FROM zeroship.permission_tokens WHERE id = $1", &[&pat.token_id]).await;
        let _ = pg.execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&pat.user_id]).await;
        let _ = pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&pat.user_id]).await;
    }
    let _ = pg.execute("DELETE FROM zeroship.users WHERE id = ANY($1)", &[&creator_ids.to_vec()]).await;
}

fn full_router() -> impl Fn(&mut web::ServiceConfig) + Clone {
    |cfg: &mut web::ServiceConfig| {
        cfg.service(
            web::resource("/api/apps/{id}/invoices").route(web::get().to(api::list_app_invoices)),
        )
        .service(
            web::resource("/api/apps/{id}/projected-charge")
                .route(web::get().to(api::get_projected_charge)),
        )
        .service(
            web::resource("/api/apps/{id}/billing-status")
                .route(web::get().to(api::get_billing_status)),
        )
        .service(web::resource("/api/invoices/{id}").route(web::get().to(api::get_invoice)))
        .service(
            web::resource("/api/billing/credit-balance")
                .route(web::get().to(api::get_credit_balance)),
        )
        .service(
            web::resource("/api/billing/payment-method")
                .route(web::get().to(api::get_payment_method)),
        );
    }
}

// ==========================================================================
// (a) + (c): creator reads only their OWN invoices; cross-creator → 403/empty;
//            operator (Resource::Any) reads any.
// ==========================================================================

#[compio::test]
async fn invoice_history_is_creator_scoped_operator_sees_any() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_test_state(&url, "history").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 1000, 0, FX_SCALE as i64, 0).await;

    // Creator A owns app A; creator B owns app B. Disjoint billing.
    let creator_a = make_user(&pg, "creatorA").await;
    let creator_b = make_user(&pg, "creatorB").await;
    ensure_creator_billing(&pg, &creator_a, true).await;
    ensure_creator_billing(&pg, &creator_b, false).await;
    let app_a = make_app(&fx.state.registry, &plan, &creator_a).await;
    let app_b = make_app(&fx.state.registry, &plan, &creator_b).await;

    let weights = MetricWeights::new();
    let price = PlanPrice {
        base_fee_cents: 1000,
        included_units: 0,
        fx_pico_cents_per_unit: Some(FX_SCALE as u64),
        spend_limit_default_cents: 0,
    };
    let usage = std::collections::HashMap::new();
    let (inv_a, amt_a) =
        seed_finalized_invoice(&pg, &creator_a, &app_a, &plan, period, &price, &usage, &weights)
            .await;
    let (_inv_b, _amt_b) =
        seed_finalized_invoice(&pg, &creator_b, &app_b, &plan, period, &price, &usage, &weights)
            .await;

    // PATs: creator A (read on app A), creator B (read on app B), operator.
    let pat_a = issue_pat(&fx.state, creator_a, None, billing_read_on_app(app_a)).await;
    let pat_b = issue_pat(&fx.state, creator_b, None, billing_read_on_app(app_b)).await;
    let op_user = make_user(&pg, "operator").await;
    let pat_op = issue_pat(&fx.state, op_user, Some("billing"), billing_read_any()).await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;

    let get = |bearer: String, uri: String| {
        test::TestRequest::get().uri(&uri).header("authorization", bearer).to_request()
    };

    // Creator A reads app A's invoices → their own bill is present.
    let body: serde_json::Value =
        test::read_response_json(&svc, get(pat_a.bearer(), format!("/api/apps/{app_a}/invoices")))
            .await;
    let invoices = body["invoices"].as_array().expect("invoices array");
    assert_eq!(invoices.len(), 1, "creator A sees exactly their own invoice");
    assert_eq!(invoices[0]["id"], inv_a, "and it is their invoice");
    assert_eq!(invoices[0]["total_cents"], amt_a);
    assert_eq!(invoices[0]["status"], "finalized");

    // (c) Cross-creator: creator A reading app B → 403 (not their app).
    let resp = test::call_service(
        &svc,
        get(pat_a.bearer(), format!("/api/apps/{app_b}/invoices")),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "creator A must NOT read creator B's app invoices",
    );

    // Operator reads BOTH apps' invoices.
    for app in [app_a, app_b] {
        let resp = test::call_service(
            &svc,
            get(pat_op.bearer(), format!("/api/apps/{app}/invoices")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "operator reads any app's invoices");
    }

    cleanup(&pg, &[creator_a, creator_b, op_user], &[app_a, app_b], &[&pat_a, &pat_b, &pat_op]).await;
}

// ==========================================================================
// (b): an app/unauthorized token → 403 on the billing reads.
// ==========================================================================

#[compio::test]
async fn unauthorized_token_is_forbidden_on_billing_reads() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_test_state(&url, "unauth").await;
    let pg = fx.state.control_pg.clone();

    let plan = seed_plan(&pg, 0, 0, FX_SCALE as i64, 0).await;
    let creator = make_user(&pg, "creator").await;
    ensure_creator_billing(&pg, &creator, false).await;
    let app = make_app(&fx.state.registry, &plan, &creator).await;

    // A token with NO billing grant whatsoever.
    let stranger = make_user(&pg, "stranger").await;
    let pat_none = issue_pat(&fx.state, stranger, None, empty_policy()).await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;
    let get = |uri: String| {
        test::TestRequest::get()
            .uri(&uri)
            .header("authorization", pat_none.bearer())
            .to_request()
    };

    // App-scoped reads → 403.
    for uri in [
        format!("/api/apps/{app}/invoices"),
        format!("/api/apps/{app}/projected-charge"),
        format!("/api/apps/{app}/billing-status"),
    ] {
        let resp = test::call_service(&svc, get(uri.clone())).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "no-billing token: 403 on {uri}");
    }
    // Creator-keyed reads → 403 (the caller has no billing capability anywhere).
    for uri in ["/api/billing/credit-balance", "/api/billing/payment-method"] {
        let resp = test::call_service(&svc, get(uri.to_string())).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "no-billing token: 403 on {uri}");
    }

    cleanup(&pg, &[creator, stranger], &[app], &[&pat_none]).await;
}

// ==========================================================================
// (d): invoice line detail returns the FROZEN snapshot that reproduces amount.
// ==========================================================================

#[compio::test]
async fn invoice_line_detail_reproduces_amount_from_frozen_snapshot() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_test_state(&url, "linedetail").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    // A real overage: base 500c, 100 included CU, FX = 1 cent/CU, 350 CU used ⇒
    // 250 billable CU × 1c = 250c overage + 500c base = 750c.
    let plan = seed_plan(&pg, 500, 100, FX_SCALE as i64, 0).await;
    let creator = make_user(&pg, "creator").await;
    ensure_creator_billing(&pg, &creator, true).await;
    let app = make_app(&fx.state.registry, &plan, &creator).await;

    let mut weights = MetricWeights::new();
    weights.insert("rpc_calls".to_string(), MetricWeight { units_per_op: 1, per_units: 1 });
    let mut usage = std::collections::HashMap::new();
    usage.insert("rpc_calls".to_string(), 350_i64);
    let price = PlanPrice {
        base_fee_cents: 500,
        included_units: 100,
        fx_pico_cents_per_unit: Some(FX_SCALE as u64),
        spend_limit_default_cents: 0,
    };
    let (inv, amt) =
        seed_finalized_invoice(&pg, &creator, &app, &plan, period, &price, &usage, &weights).await;
    assert_eq!(amt, 750, "sanity: seeded amount is the overage+base total");

    let pat = issue_pat(&fx.state, creator, None, billing_read_on_app(app)).await;
    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;

    let body: serde_json::Value = test::read_response_json(
        &svc,
        test::TestRequest::get()
            .uri(&format!("/api/invoices/{inv}"))
            .header("authorization", pat.bearer())
            .to_request(),
    )
    .await;

    assert_eq!(body["total_cents"], 750);
    let lines = body["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 1, "one frozen segment line");
    let line = &lines[0];
    assert_eq!(line["segment_no"], 0);
    assert_eq!(line["plan_id"], plan);
    assert_eq!(line["amount_cents"], 750);

    // FAITHFUL reproducibility: re-run charge_cents over the FROZEN snapshot and
    // assert it equals the frozen amount_cents — the line replays bit-for-bit.
    let snap_usage: std::collections::HashMap<String, i64> =
        serde_json::from_value(line["usage_snapshot"].clone()).expect("usage snapshot");
    let snap_weights: MetricWeights =
        serde_json::from_value(line["weights_snapshot"].clone()).expect("weights snapshot");
    let frozen_fx = line["fx_pico_cents_per_unit"].as_i64().expect("fx") as u64;
    let frozen_included = line["included_units"].as_i64().expect("included") as u64;
    let frozen_base = line["base_fee_cents"].as_i64().expect("base") as u64;
    let replay_price = PlanPrice {
        base_fee_cents: frozen_base,
        included_units: frozen_included,
        fx_pico_cents_per_unit: Some(frozen_fx),
        spend_limit_default_cents: 0,
    };
    let replay = charge_cents(&replay_price, &snap_usage, &snap_weights).expect("replay");
    assert_eq!(
        i64::try_from(replay.total_cents).unwrap(),
        line["amount_cents"].as_i64().unwrap(),
        "the frozen snapshot reproduces amount_cents via charge_cents",
    );

    // billing-metering read-API parity: the line detail surfaces the SAME CU the
    // Stripe invoice line shows (derived from the frozen snapshot), so the
    // dashboard agrees with Stripe.
    assert_eq!(
        line["compute_units"].as_u64(),
        Some(replay.total_units),
        "read-API compute_units == the frozen snapshot's CU (matches the Stripe line)",
    );
    assert_eq!(
        line["billable_units"].as_u64(),
        Some(replay.billable_units),
        "read-API billable_units == post-included-units CU",
    );

    cleanup(&pg, &[creator], &[app], &[&pat]).await;
}

// ==========================================================================
// (e): projected-charge is labelled non-authoritative AND the cache budget
//      holds (a 2nd call within the TTL does NOT re-price).
// ==========================================================================

#[compio::test]
async fn projected_charge_is_non_authoritative_and_cache_budget_holds() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_test_state(&url, "projected").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    // Plan: base 200c, 0 included, FX 1c/CU. Weight: 1 CU per rpc_call.
    let plan = seed_plan(&pg, 200, 0, FX_SCALE as i64, 0).await;
    ensure_global_pricing(&pg, "rpc_calls", 1, 1).await;
    let creator = make_user(&pg, "creator").await;
    ensure_creator_billing(&pg, &creator, false).await;
    let app = make_app(&fx.state.registry, &plan, &creator).await;
    // 300 live rpc_calls ⇒ 300 CU × 1c + 200c base = 500c projected.
    seed_usage(&pg, &app, period, "rpc_calls", 300).await;

    let pat = issue_pat(&fx.state, creator, None, billing_read_on_app(app)).await;
    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;

    let cache = fx.state.projected_charge_cache.clone();
    let before = cache.reprice_count();

    let get = || {
        test::TestRequest::get()
            .uri(&format!("/api/apps/{app}/projected-charge"))
            .header("authorization", pat.bearer())
            .to_request()
    };

    // First call: cache MISS → one re-price.
    let body1: serde_json::Value = test::read_response_json(&svc, get()).await;
    assert_eq!(body1["authoritative"], false, "projection is NON-authoritative");
    assert_eq!(body1["projected_charge_cents"], 500, "300 CU × 1c + 200c base");
    assert_eq!(body1["period"], period.to_string());
    let after_first = cache.reprice_count();
    assert_eq!(after_first, before + 1, "first call re-prices exactly once");

    // Second call within the TTL: cache HIT → reprice_count UNCHANGED.
    let body2: serde_json::Value = test::read_response_json(&svc, get()).await;
    assert_eq!(body2["projected_charge_cents"], 500, "served from cache");
    assert_eq!(body2["authoritative"], false);
    assert_eq!(
        cache.reprice_count(),
        after_first,
        "a 2nd call within the TTL must NOT re-run pricing (cache budget holds)",
    );
    // The cached value's as_of is stable across the two calls (same compute).
    assert_eq!(body1["as_of"], body2["as_of"], "as_of is the original compute instant");

    cleanup(&pg, &[creator], &[app], &[&pat]).await;
}

// ==========================================================================
// (f): credit balance reflects grants/consumes; cross-creator isolation;
//      payment-method status; plan/spend-state.
// ==========================================================================

#[compio::test]
async fn credit_balance_pm_and_billing_status_are_creator_scoped() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_test_state(&url, "credit").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 0, 0, FX_SCALE as i64, 2500).await;
    let creator_a = make_user(&pg, "creditA").await;
    let creator_b = make_user(&pg, "creditB").await;
    ensure_creator_billing(&pg, &creator_a, true).await;
    ensure_creator_billing(&pg, &creator_b, false).await;
    let app_a = make_app(&fx.state.registry, &plan, &creator_a).await;
    let app_b = make_app(&fx.state.registry, &plan, &creator_b).await;
    seed_customer_ref(&pg, &creator_a, &format!("cus_{}", Uuid::new_v4().simple())).await;

    // Creator A: grant $50, then a finalized invoice consumes $20 → balance $30.
    zeroship_control::credit::grant(
        &*pg,
        &creator_a,
        5000,
        "usd",
        "promo",
        None,
        Some("seed grant A"),
        &format!("idem-{}", Uuid::new_v4().simple()),
    )
    .await
    .expect("grant A");
    // Seed a consume entry referencing the grant (faithful negative companion).
    let grant_id: String = pg
        .query_one(
            "SELECT id FROM zeroship.credit_ledger WHERE creator_id = $1 AND kind = 'promo' LIMIT 1",
            &[&creator_a],
        )
        .await
        .expect("read grant id")
        .get("id");
    // A finalized invoice the consume can reference.
    let (inv_a, _amt) = seed_finalized_invoice(
        &pg,
        &creator_a,
        &app_a,
        &plan,
        period,
        &PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(FX_SCALE as u64),
            spend_limit_default_cents: 2500,
        },
        &std::collections::HashMap::new(),
        &MetricWeights::new(),
    )
    .await;
    pg.execute(
        "INSERT INTO zeroship.credit_ledger \
           (id, creator_id, kind, amount_cents, currency, applied_invoice_id, consumed_from_grant_id) \
         VALUES ($1, $2, 'consumed', -2000, 'usd', $3, $4)",
        &[
            &zeroship_core::typed_id::new_credit_id(),
            &creator_a,
            &inv_a,
            &grant_id,
        ],
    )
    .await
    .expect("seed consume");

    // Creator B: grant $10 only → balance $10 (must NOT leak into A's read).
    zeroship_control::credit::grant(
        &*pg,
        &creator_b,
        1000,
        "usd",
        "grant",
        None,
        Some("seed grant B"),
        &format!("idem-{}", Uuid::new_v4().simple()),
    )
    .await
    .expect("grant B");

    let pat_a = issue_pat(&fx.state, creator_a, None, billing_read_on_app(app_a)).await;
    let pat_b = issue_pat(&fx.state, creator_b, None, billing_read_on_app(app_b)).await;
    let op_user = make_user(&pg, "operator").await;
    let pat_op = issue_pat(&fx.state, op_user, Some("billing"), billing_read_any()).await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;
    let get = |bearer: String, uri: String| {
        test::TestRequest::get().uri(&uri).header("authorization", bearer).to_request()
    };

    // (f) Creator A's balance = $50 grant − $20 consumed = $30 (3000c), and the
    // ledger shows BOTH the grant and the consume.
    let bal_a: serde_json::Value = test::read_response_json(
        &svc,
        get(pat_a.bearer(), "/api/billing/credit-balance".to_string()),
    )
    .await;
    assert_eq!(bal_a["balance_cents"], 3000, "A: $50 granted − $20 consumed = $30");
    assert_eq!(bal_a["currency"], "usd");
    let kinds: Vec<&str> = bal_a["recent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"promo"), "grant entry surfaced");
    assert!(kinds.contains(&"consumed"), "consume entry surfaced");

    // Cross-creator isolation: creator B's read shows ONLY $10, never A's $30.
    let bal_b: serde_json::Value = test::read_response_json(
        &svc,
        get(pat_b.bearer(), "/api/billing/credit-balance".to_string()),
    )
    .await;
    assert_eq!(bal_b["balance_cents"], 1000, "B sees ONLY their own $10");

    // (c) Operator may target ANY creator via ?creator_id; sees A's $30.
    let bal_op: serde_json::Value = test::read_response_json(
        &svc,
        get(
            pat_op.bearer(),
            format!("/api/billing/credit-balance?creator_id={creator_a}"),
        ),
    )
    .await;
    assert_eq!(bal_op["balance_cents"], 3000, "operator reads A's balance via ?creator_id");

    // A non-operator passing ANOTHER creator's id is 403 (no cross-creator read).
    let resp = test::call_service(
        &svc,
        get(
            pat_b.bearer(),
            format!("/api/billing/credit-balance?creator_id={creator_a}"),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "creator B must NOT read creator A's balance via ?creator_id",
    );

    // Payment-method status: A has default_pm_set + a customer ref (presence only).
    let pm_a: serde_json::Value = test::read_response_json(
        &svc,
        get(pat_a.bearer(), "/api/billing/payment-method".to_string()),
    )
    .await;
    assert_eq!(pm_a["default_pm_set"], true);
    assert_eq!(pm_a["customer_ref_present"], true);
    assert!(pm_a.get("external_id").is_none(), "raw provider id is NEVER leaked");
    // B has neither.
    let pm_b: serde_json::Value = test::read_response_json(
        &svc,
        get(pat_b.bearer(), "/api/billing/payment-method".to_string()),
    )
    .await;
    assert_eq!(pm_b["default_pm_set"], false);
    assert_eq!(pm_b["customer_ref_present"], false);

    // Billing-status for app A: plan + spend cap + states.
    let bs_a: serde_json::Value = test::read_response_json(
        &svc,
        get(pat_a.bearer(), format!("/api/apps/{app_a}/billing-status")),
    )
    .await;
    assert_eq!(bs_a["plan_id"], plan);
    assert_eq!(bs_a["plan_default_cents"], 2500);
    assert_eq!(bs_a["effective_limit_cents"], 2500, "no override ⇒ plan default");
    assert_eq!(bs_a["account_state"], "active", "no dunning row ⇒ active");
    assert_eq!(bs_a["spend_state"], "allow", "no spend-state row ⇒ allow");
    // Cross-creator: A cannot read B's billing-status.
    let resp = test::call_service(
        &svc,
        get(pat_a.bearer(), format!("/api/apps/{app_b}/billing-status")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "A cannot read B's billing-status");

    cleanup(
        &pg,
        &[creator_a, creator_b, op_user],
        &[app_a, app_b],
        &[&pat_a, &pat_b, &pat_op],
    )
    .await;
}

// ==========================================================================
// (d-authz): invoice line detail is authz-scoped by the invoice's creator —
//            a different creator cannot read it.
// ==========================================================================

#[compio::test]
async fn invoice_detail_denies_a_different_creator() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_test_state(&url, "invdetail-authz").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 100, 0, FX_SCALE as i64, 0).await;
    let creator_a = make_user(&pg, "ownerA").await;
    let creator_b = make_user(&pg, "ownerB").await;
    ensure_creator_billing(&pg, &creator_a, false).await;
    ensure_creator_billing(&pg, &creator_b, false).await;
    let app_a = make_app(&fx.state.registry, &plan, &creator_a).await;
    let app_b = make_app(&fx.state.registry, &plan, &creator_b).await;

    let price = PlanPrice {
        base_fee_cents: 100,
        included_units: 0,
        fx_pico_cents_per_unit: Some(FX_SCALE as u64),
        spend_limit_default_cents: 0,
    };
    let (inv_a, _) = seed_finalized_invoice(
        &pg,
        &creator_a,
        &app_a,
        &plan,
        period,
        &price,
        &std::collections::HashMap::new(),
        &MetricWeights::new(),
    )
    .await;

    let pat_a = issue_pat(&fx.state, creator_a, None, billing_read_on_app(app_a)).await;
    let pat_b = issue_pat(&fx.state, creator_b, None, billing_read_on_app(app_b)).await;
    let op_user = make_user(&pg, "operator").await;
    let pat_op = issue_pat(&fx.state, op_user, Some("billing"), billing_read_any()).await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;
    let get = |bearer: String, uri: String| {
        test::TestRequest::get().uri(&uri).header("authorization", bearer).to_request()
    };

    // Owner A reads their own invoice detail → 200.
    let resp = test::call_service(&svc, get(pat_a.bearer(), format!("/api/invoices/{inv_a}"))).await;
    assert_eq!(resp.status(), StatusCode::OK, "owner reads own invoice detail");

    // Creator B (owns only app B) reading A's invoice → 403.
    let resp = test::call_service(&svc, get(pat_b.bearer(), format!("/api/invoices/{inv_a}"))).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a different creator must NOT read A's invoice line detail",
    );

    // Operator reads any invoice detail → 200.
    let resp = test::call_service(&svc, get(pat_op.bearer(), format!("/api/invoices/{inv_a}"))).await;
    assert_eq!(resp.status(), StatusCode::OK, "operator reads any invoice detail");

    cleanup(
        &pg,
        &[creator_a, creator_b, op_user],
        &[app_a, app_b],
        &[&pat_a, &pat_b, &pat_op],
    )
    .await;
}

// ==========================================================================
// (CRITICAL-1): cross-creator invoice read via SHARED app membership.
//
// Attacker A owns app Z; victim-creator C is merely a VIEWER on Z. C owns app C
// and has their own invoice. Pre-fix, `get_invoice` looped
// `list_apps_for_owner(C)` (role-AGNOSTIC → includes Z because C is a member of
// Z in ANY role) and accepted A's `BillingRead` on Z, leaking C's ENTIRE invoice
// to A. The fix makes the read creator-LEVEL (caller == invoice.creator_id ||
// operator), so A → 403. RED pre-fix: A got 200 and C's invoice body.
// ==========================================================================

#[compio::test]
async fn invoice_read_denied_via_shared_app_membership() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_test_state(&url, "shared-membership").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 100, 0, FX_SCALE as i64, 0).await;

    // Attacker A owns app Z. Victim-creator C owns app C (and an invoice).
    let attacker_a = make_user(&pg, "attackerA").await;
    let victim_c = make_user(&pg, "victimC").await;
    ensure_creator_billing(&pg, &attacker_a, false).await;
    ensure_creator_billing(&pg, &victim_c, false).await;
    let app_z = make_app(&fx.state.registry, &plan, &attacker_a).await;
    let app_c = make_app(&fx.state.registry, &plan, &victim_c).await;

    // The shared-membership hole: victim-creator C is a (non-owner) VIEWER on Z.
    add_member(&pg, &app_z, &victim_c, "viewer").await;

    // C's own finalized invoice (creator-keyed to C, lines on app C).
    let price = PlanPrice {
        base_fee_cents: 100,
        included_units: 0,
        fx_pico_cents_per_unit: Some(FX_SCALE as u64),
        spend_limit_default_cents: 0,
    };
    let (inv_c, _amt) = seed_finalized_invoice(
        &pg,
        &victim_c,
        &app_c,
        &plan,
        period,
        &price,
        &std::collections::HashMap::new(),
        &MetricWeights::new(),
    )
    .await;

    // Attacker A holds BillingRead on the app they own (Z) — nothing more.
    let pat_a = issue_pat(&fx.state, attacker_a, None, billing_read_on_app(app_z)).await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;
    let get = |bearer: String, uri: String| {
        test::TestRequest::get().uri(&uri).header("authorization", bearer).to_request()
    };

    // A requests C's invoice. Pre-fix this was 200 (leak); the fix denies it.
    let resp = test::call_service(&svc, get(pat_a.bearer(), format!("/api/invoices/{inv_c}"))).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "attacker who shares an app with the victim must NOT read the victim's invoice",
    );

    cleanup(
        &pg,
        &[attacker_a, victim_c],
        &[app_z, app_c],
        &[&pat_a],
    )
    .await;
}

// ==========================================================================
// (over-disclosure): a NON-OWNER member of the owner's app must NOT read the
// owner's cross-app invoice HISTORY via `GET /api/apps/{ownerApp}/invoices`.
//
// The owner O owns app O. A VIEWER V is a member of app O — the viewer creator
// policy DOES grant `billing:read` on the app (policies/creator/app_viewer.cedar),
// so V clears the `BillingRead on App{O}` gate, but V is NOT the owner. Pre-fix
// the handler gated ONLY that capability and returned
// `list_invoices_for_creator(owner_of_app(O))` = O's whole history, so V saw O's
// billing envelope. The fix requires owner==principal || operator → V gets 403.
// ==========================================================================

#[compio::test]
async fn app_invoice_history_denied_to_non_owner_member() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_test_state(&url, "history-nonowner").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 100, 0, FX_SCALE as i64, 0).await;

    let owner = make_user(&pg, "ownerO").await;
    let viewer = make_user(&pg, "viewerV").await;
    ensure_creator_billing(&pg, &owner, false).await;
    let app_o = make_app(&fx.state.registry, &plan, &owner).await;

    // Viewer V is a non-owner member of app O; the viewer policy grants
    // billing:read on the app, so V clears the per-app gate but is not the owner.
    add_member(&pg, &app_o, &viewer, "viewer").await;

    let price = PlanPrice {
        base_fee_cents: 100,
        included_units: 0,
        fx_pico_cents_per_unit: Some(FX_SCALE as u64),
        spend_limit_default_cents: 0,
    };
    let (inv_o, _amt) = seed_finalized_invoice(
        &pg,
        &owner,
        &app_o,
        &plan,
        period,
        &price,
        &std::collections::HashMap::new(),
        &MetricWeights::new(),
    )
    .await;

    // Both PATs carry BillingRead on app O; the difference is owner-vs-member.
    let pat_owner = issue_pat(&fx.state, owner, None, billing_read_on_app(app_o)).await;
    let pat_viewer = issue_pat(&fx.state, viewer, None, billing_read_on_app(app_o)).await;
    let op_user = make_user(&pg, "operator").await;
    let pat_op = issue_pat(&fx.state, op_user, Some("billing"), billing_read_any()).await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;
    let get = |bearer: String, uri: String| {
        test::TestRequest::get().uri(&uri).header("authorization", bearer).to_request()
    };

    // Owner reads their own app's invoice history → 200 with their invoice.
    let body: serde_json::Value = test::read_response_json(
        &svc,
        get(pat_owner.bearer(), format!("/api/apps/{app_o}/invoices")),
    )
    .await;
    let invoices = body["invoices"].as_array().expect("invoices array");
    assert_eq!(invoices.len(), 1, "owner sees their own invoice");
    assert_eq!(invoices[0]["id"], inv_o);

    // First prove the per-app gate is actually CLEARED by the viewer (so the 403
    // below is the OWNER-grain check firing, not merely the capability gate): the
    // viewer reads the app's billing-STATUS (app-scoped, BillingRead) → 200.
    let resp = test::call_service(
        &svc,
        get(pat_viewer.bearer(), format!("/api/apps/{app_o}/billing-status")),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "sanity: viewer DOES hold BillingRead on app O (clears the per-app gate)",
    );

    // Non-owner viewer V (BillingRead on app O) → 403 on the OWNER's invoice
    // history. Pre-fix this was 200 and leaked the owner's whole history.
    let resp = test::call_service(
        &svc,
        get(pat_viewer.bearer(), format!("/api/apps/{app_o}/invoices")),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a non-owner member must NOT read the owner's invoice history",
    );

    // Operator reads the owner's history → 200 (no regression).
    let resp = test::call_service(
        &svc,
        get(pat_op.bearer(), format!("/api/apps/{app_o}/invoices")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "operator reads any app's invoice history");

    cleanup(
        &pg,
        &[owner, viewer, op_user],
        &[app_o],
        &[&pat_owner, &pat_viewer, &pat_op],
    )
    .await;
}
