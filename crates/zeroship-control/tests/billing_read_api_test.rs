//! PR-7 (billing-ops gap #26) — organization billing READ APIs, FAITHFUL suite.
//!
//! Drives the REAL ntex handlers (`api::list_app_invoices`, `get_invoice`,
//! `get_projected_charge`, `get_credit_balance`, `get_payment_method`,
//! `get_billing_status`) through a REAL `AuthzGuard` (real BillingRead PATs) and
//! the REAL `crate::billing_read` data layer against a live, migrated Postgres.
//! NO shims: the same router wiring, the same Cedar enforce, the same
//! `charge_cents` pricing kernel the reconciler runs.
//!
//! The SECURITY core (a organization must never read another organization's billing):
//!   - a organization reads ONLY their own apps' invoices/lines/credit;
//!   - a SECOND organization's data is invisible (cross-organization read → 403/empty);
//!   - an unauthorized / wrong-app token → 403;
//!   - an operator (`Resource::Any`) reads any organization;
//!   - the invoice line detail returns the FROZEN snapshot that reproduces
//!     `amount_cents` via `charge_cents`;
//!   - the projected-charge response is labelled non-authoritative AND the cache
//!     budget holds (a 2nd call within the TTL does NOT re-price — asserted via
//!     the cache's reprice counter, faithfully);
//!   - the credit balance reflects grants/consumes.
//!
//! PARALLEL-SAFE (the PR-6 lesson): every test mints UNIQUE creators/apps and
//! asserts per-organization scope, so the default cargo runner can run them
//! concurrently without cross-test interference.
//!
//! Configure a test database (`zeroship_core::config::test_database_url_opt`;
//! run `tests/provision_test_backends.sh` to provision one) to run; silently
//! skips otherwise.

#![allow(clippy::future_not_send)]

use crate::common;

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use compio_postgres::{connect, Client, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::pricing::{charge_cents, MetricWeight, MetricWeights, PlanPrice, FX_SCALE};
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> String {
    common::require_control_db()
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
        worker_enrolment: zeroship_control::worker_enrolment::EnrolmentEnvelope::closed(),
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
        mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
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
// Bearer minting helpers
// --------------------------------------------------------------------------

struct Caller {
    user_id: Uuid,
    token: String,
}

impl Caller {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

/// Issue a platform OAuth bearer for `user_id` carrying `scope`.
///
/// It used to take an optional platform role and seed a `platform_admin_roles`
/// row for the operator paths. That table and those roles are deleted, so every
/// principal this mints is an ordinary organization.
async fn issue_bearer(state: &AppState, user_id: Uuid, scope: &str) -> Caller {
    let _ = state;
    Caller {
        user_id,
        token: common::platform_token_for_client(user_id, scope, common::CONSOLE_CLIENT_ID),
    }
}

// The scope vocabulary is resource-blind: a scope always lowers to
// `Resource::Any`, so per-app narrowing now comes from Cedar app membership
// rather than from the caller-supplied wrapper policy a PAT used to carry.

// --------------------------------------------------------------------------
// Seeding helpers (faithful: real schema, real pricing inputs)
// --------------------------------------------------------------------------

/// The organization an app bills, read back off the app row.
///
/// The subject is derived from the app rather than minted beside it. Minting one
/// separately would produce a well-formed organization that owns nothing, and
/// every scope assertion below would then pass against an empty set - the
/// failure mode a fixture must not be able to have.
async fn app_organization(pg: &Client, app: &Uuid) -> String {
    pg.query(
        "SELECT organization_id FROM zeroship.apps WHERE id = $1",
        &[app],
    )
    .await
    .expect("read app organization")
    .first()
    .expect("app exists")
    .get("organization_id")
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

/// Create an app in `owner`'s personal organization (minted on demand).
async fn make_app(registry: &Registry, plan_id: &str, owner: &Uuid) -> Uuid {
    registry
        .create_app(&format!("read-{}", Uuid::new_v4().simple()), plan_id, owner, None)
        .await
        .expect("create app")
        .id
}

/// Seat `user` at a NON-owner role in the ORGANIZATION that owns `app`'s
/// project. Used to model the cross-organization hole: a victim-organization who is
/// merely a viewer of the attacker's app appears in the attacker's
/// role-AGNOSTIC `list_apps_for_owner`.
///
/// App-level membership is gone; an app's authority is its organization seat,
/// reached through `apps.project_id -> projects.organization_id`. That makes
/// the hole this models WIDER, not narrower: seating a viewer now exposes every
/// app of the organization rather than one, which is exactly what the assertion
/// downstream has to keep refusing.
async fn add_member(pg: &Client, app: &Uuid, user: &Uuid, role: &str) {
    pg.execute(
        "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
         SELECT p.organization_id, $2, $3 \
           FROM zeroship.apps a \
           JOIN zeroship.projects p ON p.id = a.project_id \
          WHERE a.id = $1 \
         ON CONFLICT (organization_id, user_id) DO UPDATE SET role = EXCLUDED.role",
        &[app, user, &role],
    )
    .await
    .expect("seat non-owner organization member");
}

/// Ensure a `organization_billing` row exists (the FK target for invoices/credit).
async fn ensure_organization_billing(pg: &Client, organization_id: &str, default_pm_set: bool) {
    pg.execute(
        "INSERT INTO zeroship.organization_billing (organization_id, default_pm_set) \
         VALUES ($1, $2) \
         ON CONFLICT (organization_id) DO UPDATE SET default_pm_set = EXCLUDED.default_pm_set",
        &[&organization_id, &default_pm_set],
    )
    .await
    .expect("ensure organization_billing");
}

/// Seed a Stripe customer ref (presence only is surfaced; the raw id stays internal).
async fn seed_customer_ref(pg: &Client, organization_id: &str, external_id: &str) {
    pg.execute(
        "INSERT INTO zeroship.billing_customer_refs (organization_id, provider, external_id) \
         VALUES ($1, 'stripe', $2) ON CONFLICT DO NOTHING",
        &[&organization_id, &external_id],
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
    organization_id: &str,
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
           (id, organization_id, period, status, currency, subtotal_cents, credit_cents, tax_cents, total_cents) \
         VALUES ($1, $2, $3::date, 'draft', 'usd', $4, 0, 0, $4)",
        &[&invoice_id, &organization_id, &period, &amount],
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

async fn cleanup(pg: &Client, organization_ids: &[Uuid], app_ids: &[Uuid], callers: &[&Caller]) {
    for app in app_ids {
        let _ = pg.execute("DELETE FROM zeroship.usage_aggregates WHERE app_id = $1", &[app]).await;
        let _ = pg.execute("DELETE FROM zeroship.invoice_lines WHERE app_id = $1", &[app]).await;
        let _ = pg.execute("DELETE FROM zeroship.app_spend_state WHERE app_id = $1", &[app]).await;
        let _ = pg.execute("DELETE FROM zeroship.app_spend_limit WHERE app_id = $1", &[app]).await;
        let _ = pg.execute("DELETE FROM zeroship.organization_members om USING zeroship.apps a JOIN zeroship.projects p ON p.id = a.project_id WHERE om.organization_id = p.organization_id AND a.id = $1", &[app]).await;
    }
    for organization in organization_ids {
        // Invoices reference lines (RESTRICT) — lines were dropped above by app_id,
        // but drop any remaining by organization before the invoice rows.
        let _ = pg
            .execute(
                "DELETE FROM zeroship.invoice_lines WHERE invoice_id IN \
                 (SELECT id FROM zeroship.invoices WHERE organization_id = $1)",
                &[organization],
            )
            .await;
        let _ = pg.execute("DELETE FROM zeroship.credit_ledger WHERE organization_id = $1", &[organization]).await;
        let _ = pg.execute("DELETE FROM zeroship.invoices WHERE organization_id = $1", &[organization]).await;
        let _ = pg.execute("DELETE FROM zeroship.billing_customer_refs WHERE organization_id = $1", &[organization]).await;
        let _ = pg.execute("DELETE FROM zeroship.organization_billing_status WHERE organization_id = $1", &[organization]).await;
        let _ = pg.execute("DELETE FROM zeroship.organization_billing WHERE organization_id = $1", &[organization]).await;
    }
    for app in app_ids {
        let _ = pg.execute("DELETE FROM zeroship.apps WHERE id = $1", &[app]).await;
    }
    for caller in callers {
        let _ = pg.execute("DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1", &[&caller.user_id]).await;
        let _ = pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&caller.user_id]).await;
    }
    let _ = pg.execute("DELETE FROM zeroship.users WHERE id = ANY($1)", &[&organization_ids.to_vec()]).await;
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
// (a) + (c): organization reads only their OWN invoices; cross-organization → 403/empty;
//            operator (Resource::Any) reads any.
// ==========================================================================

#[compio::test]
async fn invoice_history_is_creator_scoped_with_no_operator_exception() {
    let url = db_url();
    let fx = build_test_state(&url, "history").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 1000, 0, FX_SCALE as i64, 0).await;

    // Creator A owns app A; organization B owns app B. Disjoint billing.
    let user_a = make_user(&pg, "creatorA").await;
    let user_b = make_user(&pg, "creatorB").await;
    let app_a = make_app(&fx.state.registry, &plan, &user_a).await;
    let organization_a = app_organization(&pg, &app_a).await;
    let organization_a = organization_a.as_str();
    ensure_organization_billing(&pg, organization_a, true).await;
    let app_b = make_app(&fx.state.registry, &plan, &user_b).await;
    let organization_b = app_organization(&pg, &app_b).await;
    let organization_b = organization_b.as_str();
    ensure_organization_billing(&pg, organization_b, false).await;

    let weights = MetricWeights::new();
    let price = PlanPrice {
        base_fee_cents: 1000,
        included_units: 0,
        fx_pico_cents_per_unit: Some(FX_SCALE as u64),
        spend_limit_default_cents: 0,
    };
    let usage = std::collections::HashMap::new();
    let (inv_a, amt_a) =
        seed_finalized_invoice(&pg, &organization_a, &app_a, &plan, period, &price, &usage, &weights)
            .await;
    let (_inv_b, _amt_b) =
        seed_finalized_invoice(&pg, &organization_b, &app_b, &plan, period, &price, &usage, &weights)
            .await;

    // PATs: organization A (read on app A), organization B (read on app B), operator.
    let pat_a = issue_bearer(&fx.state, user_a, "billing:read").await;
    let pat_b = issue_bearer(&fx.state, user_b, "billing:read").await;
    let outsider = make_user(&pg, "outsider").await;
    let pat_outsider = issue_bearer(&fx.state, outsider, "billing:read").await;

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
    assert_eq!(invoices.len(), 1, "organization A sees exactly their own invoice");
    assert_eq!(invoices[0]["id"], inv_a, "and it is their invoice");
    assert_eq!(invoices[0]["total_cents"], amt_a);
    assert_eq!(invoices[0]["status"], "finalized");

    // (c) Cross-organization: organization A reading app B → 403 (not their app).
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(
        &svc,
        get(pat_a.bearer(), format!("/api/apps/{app_b}/invoices")),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "organization A must NOT read organization B's app invoices",
    );

    // There is no operator arm left to read across creators. A third organization
    // holding the same billing scopes, but no membership of either app, is
    // refused on BOTH - which is the property the operator arm used to be the
    // documented exception to.
    for app in [app_a, app_b] {
        let status = test::call_service(
            &svc,
            get(pat_outsider.bearer(), format!("/api/apps/{app}/invoices")),
        )
        .await
        .status();
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a organization with no membership must not read another organization's app invoices",
        );
    }

    cleanup(&pg, &[user_a, user_b, outsider], &[app_a, app_b], &[&pat_a, &pat_b, &pat_outsider]).await;

    // Teardown: the ntex test service holds a cloned Arc<AppState>, and the
    // fixture holds the fixture's own Postgres connection; both locals are
    // dropped only after the body returns - by which point the runtime is gone
    // and the sockets can no longer be closed. Drop them explicitly, then wait
    // for the close to land.
    drop(svc);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}

// ==========================================================================
// (b): an app/unauthorized token → 403 on the billing reads.
// ==========================================================================

#[compio::test]
async fn unauthorized_token_is_forbidden_on_billing_reads() {
    let url = db_url();
    let fx = build_test_state(&url, "unauth").await;
    let pg = fx.state.control_pg.clone();

    let plan = seed_plan(&pg, 0, 0, FX_SCALE as i64, 0).await;
    let user = make_user(&pg, "organization").await;
    let app = make_app(&fx.state.registry, &plan, &user).await;
    let organization = app_organization(&pg, &app).await;
    let organization = organization.as_str();
    ensure_organization_billing(&pg, organization, false).await;

    // A token with NO billing grant whatsoever.
    let stranger = make_user(&pg, "stranger").await;
    let pat_none = issue_bearer(&fx.state, stranger, "apps:read").await;

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
        let status = test::call_service(&svc, get(uri.clone())).await.status();
        assert_eq!(status, StatusCode::FORBIDDEN, "no-billing token: 403 on {uri}");
    }
    // Organization-keyed reads → 403. They now NAME the organization, because
    // "the caller's own billing" stopped being a single answer the moment a user
    // could hold seats at several organizations. The refusal is therefore about
    // authority at THAT organization, not about the caller having none anywhere.
    for uri in ["/api/billing/credit-balance", "/api/billing/payment-method"] {
        let uri = format!("{uri}?organization_id={organization}");
        let status = test::call_service(&svc, get(uri.clone())).await.status();
        assert_eq!(status, StatusCode::FORBIDDEN, "no-billing token: 403 on {uri}");
    }

    cleanup(&pg, &[user, stranger], &[app], &[&pat_none]).await;

    drop(svc);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}

// ==========================================================================
// (d): invoice line detail returns the FROZEN snapshot that reproduces amount.
// ==========================================================================

#[compio::test]
async fn invoice_line_detail_reproduces_amount_from_frozen_snapshot() {
    let url = db_url();
    let fx = build_test_state(&url, "linedetail").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    // A real overage: base 500c, 100 included CU, FX = 1 cent/CU, 350 CU used ⇒
    // 250 billable CU × 1c = 250c overage + 500c base = 750c.
    let plan = seed_plan(&pg, 500, 100, FX_SCALE as i64, 0).await;
    let user = make_user(&pg, "organization").await;
    let app = make_app(&fx.state.registry, &plan, &user).await;
    let organization = app_organization(&pg, &app).await;
    let organization = organization.as_str();
    ensure_organization_billing(&pg, organization, true).await;

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
        seed_finalized_invoice(&pg, &organization, &app, &plan, period, &price, &usage, &weights).await;
    assert_eq!(amt, 750, "sanity: seeded amount is the overage+base total");

    let pat = issue_bearer(&fx.state, user, "billing:read").await;
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

    cleanup(&pg, &[user], &[app], &[&pat]).await;

    drop(svc);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}

// ==========================================================================
// (e): projected-charge is labelled non-authoritative AND the cache budget
//      holds (a 2nd call within the TTL does NOT re-price).
// ==========================================================================

#[compio::test]
async fn projected_charge_is_non_authoritative_and_cache_budget_holds() {
    let url = db_url();
    let fx = build_test_state(&url, "projected").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    // Plan: base 200c, 0 included, FX 1c/CU. Weight: 1 CU per rpc_call.
    let plan = seed_plan(&pg, 200, 0, FX_SCALE as i64, 0).await;
    ensure_global_pricing(&pg, "rpc_calls", 1, 1).await;
    let user = make_user(&pg, "organization").await;
    let app = make_app(&fx.state.registry, &plan, &user).await;
    let organization = app_organization(&pg, &app).await;
    let organization = organization.as_str();
    ensure_organization_billing(&pg, organization, false).await;
    // 300 live rpc_calls ⇒ 300 CU × 1c + 200c base = 500c projected.
    seed_usage(&pg, &app, period, "rpc_calls", 300).await;

    let pat = issue_bearer(&fx.state, user, "billing:read").await;
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

    cleanup(&pg, &[user], &[app], &[&pat]).await;

    drop(svc);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}

// ==========================================================================
// (f): credit balance reflects grants/consumes; cross-organization isolation;
//      payment-method status; plan/spend-state.
// ==========================================================================

#[compio::test]
async fn credit_balance_pm_and_billing_status_are_creator_scoped() {
    let url = db_url();
    let fx = build_test_state(&url, "credit").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 0, 0, FX_SCALE as i64, 2500).await;
    let user_a = make_user(&pg, "creditA").await;
    let user_b = make_user(&pg, "creditB").await;
    let app_a = make_app(&fx.state.registry, &plan, &user_a).await;
    let organization_a = app_organization(&pg, &app_a).await;
    let organization_a = organization_a.as_str();
    ensure_organization_billing(&pg, organization_a, true).await;
    let app_b = make_app(&fx.state.registry, &plan, &user_b).await;
    let organization_b = app_organization(&pg, &app_b).await;
    let organization_b = organization_b.as_str();
    ensure_organization_billing(&pg, organization_b, false).await;
    seed_customer_ref(&pg, &organization_a, &format!("cus_{}", Uuid::new_v4().simple())).await;

    // Creator A: grant $50, then a finalized invoice consumes $20 → balance $30.
    zeroship_control::credit::grant(&*pg, &organization_a, zeroship_control::credit::GrantRequest { amount_cents: 5000, currency: "usd", kind: "promo", expires_at: None, note: Some("seed grant A"), idempotency_key: &format!("idem-{}", Uuid::new_v4().simple()) })
    .await
    .expect("grant A");
    // Seed a consume entry referencing the grant (faithful negative companion).
    let grant_id: String = pg
        .query_one(
            "SELECT id FROM zeroship.credit_ledger WHERE organization_id = $1 AND kind = 'promo' LIMIT 1",
            &[&organization_a],
        )
        .await
        .expect("read grant id")
        .get("id");
    // A finalized invoice the consume can reference.
    let (inv_a, _amt) = seed_finalized_invoice(
        &pg,
        &organization_a,
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
           (id, organization_id, kind, amount_cents, currency, applied_invoice_id, consumed_from_grant_id) \
         VALUES ($1, $2, 'consumed', -2000, 'usd', $3, $4)",
        &[
            &zeroship_core::typed_id::new_credit_id(),
            &organization_a,
            &inv_a,
            &grant_id,
        ],
    )
    .await
    .expect("seed consume");

    // Creator B: grant $10 only → balance $10 (must NOT leak into A's read).
    zeroship_control::credit::grant(&*pg, &organization_b, zeroship_control::credit::GrantRequest { amount_cents: 1000, currency: "usd", kind: "grant", expires_at: None, note: Some("seed grant B"), idempotency_key: &format!("idem-{}", Uuid::new_v4().simple()) })
    .await
    .expect("grant B");

    let pat_a = issue_bearer(&fx.state, user_a, "billing:read").await;
    let pat_b = issue_bearer(&fx.state, user_b, "billing:read").await;
    let outsider = make_user(&pg, "outsider").await;
    let pat_outsider = issue_bearer(&fx.state, outsider, "billing:read").await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;
    let get = |bearer: String, uri: String| {
        test::TestRequest::get().uri(&uri).header("authorization", bearer).to_request()
    };

    // (f) Creator A's balance = $50 grant − $20 consumed = $30 (3000c), and the
    // ledger shows BOTH the grant and the consume.
    let bal_a: serde_json::Value = test::read_response_json(
        &svc,
        get(
            pat_a.bearer(),
            format!("/api/billing/credit-balance?organization_id={organization_a}"),
        ),
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

    // Cross-organization isolation: organization B's read shows ONLY $10, never A's $30.
    let bal_b: serde_json::Value = test::read_response_json(
        &svc,
        get(
            pat_b.bearer(),
            format!("/api/billing/credit-balance?organization_id={organization_b}"),
        ),
    )
    .await;
    assert_eq!(bal_b["balance_cents"], 1000, "B sees ONLY their own $10");

    // (c) `?organization_id` naming an organization the caller has no seat at is
    // 403 for everyone. The parameter is REQUIRED now rather than defaulting to
    // the caller: a user may hold seats at several organizations, so "my
    // billing" no longer names one subject. What it selects is checked against
    // the ladder's money authority at that organization, so naming a stranger's
    // organization is refused rather than silently answered with the caller's.
    let status = test::call_service(
        &svc,
        get(
            pat_outsider.bearer(),
            format!("/api/billing/credit-balance?organization_id={organization_a}"),
        ),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "no caller may read another organization's balance via ?organization_id",
    );

    // A non-operator passing ANOTHER organization's id is 403 (no cross-organization read).
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(
        &svc,
        get(
            pat_b.bearer(),
            format!("/api/billing/credit-balance?organization_id={organization_a}"),
        ),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "organization B must NOT read organization A's balance via ?organization_id",
    );

    // Payment-method status: A has default_pm_set + a customer ref (presence only).
    let pm_a: serde_json::Value = test::read_response_json(
        &svc,
        get(
            pat_a.bearer(),
            format!("/api/billing/payment-method?organization_id={organization_a}"),
        ),
    )
    .await;
    assert_eq!(pm_a["default_pm_set"], true);
    assert_eq!(pm_a["customer_ref_present"], true);
    assert!(pm_a.get("external_id").is_none(), "raw provider id is NEVER leaked");
    // B has neither.
    let pm_b: serde_json::Value = test::read_response_json(
        &svc,
        get(
            pat_b.bearer(),
            format!("/api/billing/payment-method?organization_id={organization_b}"),
        ),
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
    // Cross-organization: A cannot read B's billing-status.
    let status = test::call_service(
        &svc,
        get(pat_a.bearer(), format!("/api/apps/{app_b}/billing-status")),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::FORBIDDEN, "A cannot read B's billing-status");

    cleanup(
        &pg,
        &[user_a, user_b, outsider],
        &[app_a, app_b],
        &[&pat_a, &pat_b, &pat_outsider],
    )
    .await;

    drop(svc);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}

// ==========================================================================
// (d-authz): invoice line detail is authz-scoped by the invoice's organization —
//            a different organization cannot read it.
// ==========================================================================

#[compio::test]
async fn invoice_detail_denies_a_different_creator() {
    let url = db_url();
    let fx = build_test_state(&url, "invdetail-authz").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 100, 0, FX_SCALE as i64, 0).await;
    let user_a = make_user(&pg, "ownerA").await;
    let user_b = make_user(&pg, "ownerB").await;
    let app_a = make_app(&fx.state.registry, &plan, &user_a).await;
    let organization_a = app_organization(&pg, &app_a).await;
    let organization_a = organization_a.as_str();
    ensure_organization_billing(&pg, organization_a, false).await;
    let app_b = make_app(&fx.state.registry, &plan, &user_b).await;
    let organization_b = app_organization(&pg, &app_b).await;
    let organization_b = organization_b.as_str();
    ensure_organization_billing(&pg, organization_b, false).await;

    let price = PlanPrice {
        base_fee_cents: 100,
        included_units: 0,
        fx_pico_cents_per_unit: Some(FX_SCALE as u64),
        spend_limit_default_cents: 0,
    };
    let (inv_a, _) = seed_finalized_invoice(
        &pg,
        &organization_a,
        &app_a,
        &plan,
        period,
        &price,
        &std::collections::HashMap::new(),
        &MetricWeights::new(),
    )
    .await;

    let pat_a = issue_bearer(&fx.state, user_a, "billing:read").await;
    let pat_b = issue_bearer(&fx.state, user_b, "billing:read").await;
    let outsider = make_user(&pg, "outsider").await;
    let pat_outsider = issue_bearer(&fx.state, outsider, "billing:read").await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;
    let get = |bearer: String, uri: String| {
        test::TestRequest::get().uri(&uri).header("authorization", bearer).to_request()
    };

    // Owner A reads their own invoice detail → 200.
    // Status only: retaining a `WebResponse` binding across these three rebinds
    // would keep the app state - and its Postgres client - alive past the
    // teardown below, so every call reads only `.status()`.
    let status = test::call_service(&svc, get(pat_a.bearer(), format!("/api/invoices/{inv_a}"))).await.status();
    assert_eq!(status, StatusCode::OK, "owner reads own invoice detail");

    // Creator B (owns only app B) reading A's invoice → 403.
    let status = test::call_service(&svc, get(pat_b.bearer(), format!("/api/invoices/{inv_a}"))).await.status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a different organization must NOT read A's invoice line detail",
    );

    // A third organization is refused exactly like B. The operator arm that used to
    // read any invoice detail is deleted, so "a different organization" is now the
    // only case there is.
    let status = test::call_service(&svc, get(pat_outsider.bearer(), format!("/api/invoices/{inv_a}"))).await.status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "no caller but the invoice's own organization reads its line detail",
    );

    cleanup(
        &pg,
        &[user_a, user_b, outsider],
        &[app_a, app_b],
        &[&pat_a, &pat_b, &pat_outsider],
    )
    .await;

    drop(svc);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}

// ==========================================================================
// (CRITICAL-1): cross-organization invoice read via SHARED app membership.
//
// Attacker A owns app Z; victim-organization C is merely a VIEWER on Z. C owns app C
// and has their own invoice. Pre-fix, `get_invoice` looped
// `list_apps_for_owner(C)` (role-AGNOSTIC → includes Z because C is a member of
// Z in ANY role) and accepted A's `BillingRead` on Z, leaking C's ENTIRE invoice
// to A. The fix makes the read organization-LEVEL (caller == invoice.organization_id ||
// operator), so A → 403. RED pre-fix: A got 200 and C's invoice body.
// ==========================================================================

#[compio::test]
async fn invoice_read_denied_via_shared_app_membership() {
    let url = db_url();
    let fx = build_test_state(&url, "shared-membership").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 100, 0, FX_SCALE as i64, 0).await;

    // Attacker A owns app Z. Victim-organization C owns app C (and an invoice).
    let attacker_a = make_user(&pg, "attackerA").await;
    let victim_c = make_user(&pg, "victimC").await;
    let app_z = make_app(&fx.state.registry, &plan, &attacker_a).await;
    let app_c = make_app(&fx.state.registry, &plan, &victim_c).await;
    let organization_a = app_organization(&pg, &app_z).await;
    let organization_c = app_organization(&pg, &app_c).await;
    ensure_organization_billing(&pg, &organization_a, false).await;
    ensure_organization_billing(&pg, &organization_c, false).await;

    // The shared-membership hole: victim-organization C is a (non-owner) VIEWER on Z.
    add_member(&pg, &app_z, &victim_c, "viewer").await;

    // C's own finalized invoice (organization-keyed to C, lines on app C).
    let price = PlanPrice {
        base_fee_cents: 100,
        included_units: 0,
        fx_pico_cents_per_unit: Some(FX_SCALE as u64),
        spend_limit_default_cents: 0,
    };
    let (inv_c, _amt) = seed_finalized_invoice(
        &pg,
        &organization_c,
        &app_c,
        &plan,
        period,
        &price,
        &std::collections::HashMap::new(),
        &MetricWeights::new(),
    )
    .await;

    // Attacker A holds BillingRead on the app they own (Z) — nothing more.
    let pat_a = issue_bearer(&fx.state, attacker_a, "billing:read").await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;
    let get = |bearer: String, uri: String| {
        test::TestRequest::get().uri(&uri).header("authorization", bearer).to_request()
    };

    // A requests C's invoice. Pre-fix this was 200 (leak); the fix denies it.
    let status = test::call_service(&svc, get(pat_a.bearer(), format!("/api/invoices/{inv_c}"))).await.status();
    assert_eq!(
        status,
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

    drop(svc);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}

// ==========================================================================
// The invoice HISTORY of an organization is read by money authority at that
// organization, and by nothing else.
//
// THIS TEST'S PREMISE INVERTED, AND THE INVERSION IS THE POINT. It used to
// assert that a bookkeeper V - the `billing` seat, rank 10 / billing_rank 20 -
// was REFUSED O's cross-app history, because an invoice was keyed on one human
// and only that human could read it. The invoice is keyed on the ORGANIZATION
// now, and `billing` is the seat the ladder defines as money authority over it,
// so refusing V would be refusing the exact person the seat exists for. The
// policy file says so in as many words: "a bookkeeper reads the invoice".
//
// What still has to hold, and is what this asserts in three directions:
//   * the `billing` seat READS the history (billing_rank 20),
//   * a `developer` seat does NOT (billing_rank 0, and it holds plenty of app
//     authority - so a pass here would be app authority leaking into money),
//   * someone with no seat at all does NOT.
// ==========================================================================

#[compio::test]
async fn app_invoice_history_follows_money_authority_not_app_authority() {
    let url = db_url();
    let fx = build_test_state(&url, "history-nonowner").await;
    let pg = fx.state.control_pg.clone();
    let period = current_period();

    let plan = seed_plan(&pg, 100, 0, FX_SCALE as i64, 0).await;

    let owner = make_user(&pg, "ownerO").await;
    let bookkeeper = make_user(&pg, "bookkeeperV").await;
    let developer = make_user(&pg, "developerD").await;
    let app_o = make_app(&fx.state.registry, &plan, &owner).await;
    let organization_o = app_organization(&pg, &app_o).await;
    ensure_organization_billing(&pg, &organization_o, false).await;

    // V holds the `billing` seat: money authority, no app authority.
    // D holds `developer`: app authority, no money authority. The pair is what
    // separates the two axes.
    add_member(&pg, &app_o, &bookkeeper, "billing").await;
    add_member(&pg, &app_o, &developer, "developer").await;

    let price = PlanPrice {
        base_fee_cents: 100,
        included_units: 0,
        fx_pico_cents_per_unit: Some(FX_SCALE as u64),
        spend_limit_default_cents: 0,
    };
    let (inv_o, _amt) = seed_finalized_invoice(
        &pg,
        &organization_o,
        &app_o,
        &plan,
        period,
        &price,
        &std::collections::HashMap::new(),
        &MetricWeights::new(),
    )
    .await;

    let pat_owner = issue_bearer(&fx.state, owner, "billing:read").await;
    let pat_bookkeeper = issue_bearer(&fx.state, bookkeeper, "billing:read").await;
    let pat_developer = issue_bearer(&fx.state, developer, "billing:read").await;
    let outsider = make_user(&pg, "outsider").await;
    let pat_outsider = issue_bearer(&fx.state, outsider, "billing:read").await;

    let svc = test::init_service(web::App::new().state(fx.state.clone()).configure(full_router()))
        .await;
    let get = |bearer: String, uri: String| {
        test::TestRequest::get().uri(&uri).header("authorization", bearer).to_request()
    };

    // The owner reads it.
    let body: serde_json::Value = test::read_response_json(
        &svc,
        get(pat_owner.bearer(), format!("/api/apps/{app_o}/invoices")),
    )
    .await;
    let invoices = body["invoices"].as_array().expect("invoices array");
    assert_eq!(invoices.len(), 1, "the owner sees the organization's invoice");
    assert_eq!(invoices[0]["id"], inv_o);

    // The bookkeeper reads it: that is what billing_rank 20 IS.
    let body: serde_json::Value = test::read_response_json(
        &svc,
        get(pat_bookkeeper.bearer(), format!("/api/apps/{app_o}/invoices")),
    )
    .await;
    let invoices = body["invoices"].as_array().expect("invoices array");
    assert_eq!(invoices.len(), 1, "the billing seat reads the organization's invoices");
    assert_eq!(invoices[0]["id"], inv_o);

    // The developer does NOT, despite holding app authority. Without this arm the
    // pair above would pass just as happily with no money gate at all.
    // Status only across these rebinds: retaining a `WebResponse` binding would
    // keep the app state - and its Postgres client - alive past the teardown.
    let status = test::call_service(
        &svc,
        get(pat_developer.bearer(), format!("/api/apps/{app_o}/invoices")),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "app authority must not reach the invoice envelope",
    );

    // Nor does someone with no seat at the organization at all.
    let status = test::call_service(
        &svc,
        get(pat_outsider.bearer(), format!("/api/apps/{app_o}/invoices")),
    )
    .await
    .status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an unseated caller reads no organization's invoices",
    );

    cleanup(
        &pg,
        &[owner, bookkeeper, developer, outsider],
        &[app_o],
        &[&pat_owner, &pat_bookkeeper, &pat_developer, &pat_outsider],
    )
    .await;

    drop(svc);
    drop(pg);
    drop(fx);
    common::drain_pg().await;
}
