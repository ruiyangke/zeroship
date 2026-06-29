//! PR-2 regression tests for billing-ops gap #26: the `0048 credit_ledger`
//! append-only ledger, per-grant FIFO consume-at-finalize, and the operator
//! `POST /api/billing/credit` grant endpoint.
//!
//! FAITHFUL by construction: every assertion runs against a live, migrated Postgres
//! (`CONTROL_TEST_DB`; silent skip otherwise) and exercises the REAL paths —
//!   * the REAL `credit_ledger` table / domain / kind↔sign CHECK / immutability trigger;
//!   * the REAL reconciler `billing_reconcile::bill_creator` (so consume runs inside the
//!     real finalize-in-one-UPDATE, the real balance CHECK validates the row);
//!   * the REAL `credit::grant` / `credit::consume_at_finalize` Rust helpers;
//!   * the REAL `api::grant_credit` HTTP handler via an `ntex` test app (operator authz,
//!     idempotency-key header, body fingerprint, 403/409).
//! Only the Stripe WIRE is a recording fake (Stripe is irrelevant to credit math; the
//! Stripe path is covered by `billing_reconcile_test`).
//!
//! These FAIL against the pre-PR-2 code/schema:
//!   (a) no `credit_ledger` table → the append-only / CHECK assertions cannot even run;
//!   (b) the finalize UPDATE hard-wired `credit_cents = 0` → consume writes nothing, so
//!       `credit_cents`/`total_cents`/the per-grant `consumed` entry assertions fail;
//!   (c) a reconcile re-run with no re-run guard double-consumes → balance not conserved;
//!   (d) no expiry filter → an expired grant is drawn;
//!   (e) no `grant_credit` endpoint / no operator gate → the 403 + 409 assertions fail;
//!   (f) no currency filter → a non-USD grant is drawn against a USD bill.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_authz::{
    policy_hash, Action as AuthzAction, Effect, Policy, Resource as AuthzResource, Statement,
};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::billing_reconcile;
use zeroship_control::credit::{self, GrantOutcome};
use zeroship_control::metering::Metering;
use zeroship_control::registry::RegistryError;
use zeroship_control::stripe_client::{Period, StripeApi};
use zeroship_control::stripe_store::StripeError;
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// The reconciler single-flights fleet-wide via `pg_try_advisory_lock`; serialize
/// the reconcile-driving tests with a process-wide lock (mirrors the reconcile test).
static RECONCILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-credit-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ===========================================================================
// A recording StripeApi fake (no HTTP). `bill_creator` is generic over StripeApi;
// the credit math under test never touches Stripe, so a fake suffices and keeps
// the test focused on the REAL reconciler + REAL PG + REAL credit helpers.
// ===========================================================================

#[derive(Default)]
struct RecordingStripe {
    item_seq: Mutex<u64>,
    inv_seq: Mutex<u64>,
}

impl RecordingStripe {
    /// A process-unique id so distinct creators never collide on the
    /// `billing_provider_refs` UNIQUE(provider, ref_kind, external_id).
    fn unique(prefix: &str) -> String {
        format!("{prefix}_{}", Uuid::new_v4().simple())
    }
}

impl StripeApi for RecordingStripe {
    async fn create_customer(&self, _e: &str, _c: &str) -> Result<String, StripeError> {
        Ok("cus_fake".into())
    }
    async fn create_checkout_setup_session(
        &self,
        _c: &str,
        _ok: &str,
        _cancel: &str,
    ) -> Result<String, StripeError> {
        Ok("https://fake/session".into())
    }
    async fn create_invoice_item(
        &self,
        _customer: &str,
        _amount: u64,
        _currency: &str,
        _desc: &str,
        _period: Period,
        _idem: &str,
        _lookup: &str,
        _metadata: &[(String, String)],
    ) -> Result<String, StripeError> {
        *self.item_seq.lock().unwrap() += 1;
        Ok(Self::unique("ii"))
    }
    async fn delete_invoice_item(&self, _item_id: &str) -> Result<(), StripeError> {
        Ok(())
    }
    async fn find_invoice_item_by_key(
        &self,
        _c: &str,
        _k: &str,
    ) -> Result<Option<String>, StripeError> {
        Ok(None)
    }
    async fn create_invoice(&self, _c: &str, _cr: &str, _k: &str) -> Result<String, StripeError> {
        *self.inv_seq.lock().unwrap() += 1;
        Ok(Self::unique("in"))
    }
    async fn finalize_invoice(&self, id: &str) -> Result<String, StripeError> {
        // finalize does not change the invoice id — echo the draft id back.
        Ok(id.to_string())
    }
    async fn create_meter_event(
        &self,
        _n: &str,
        _c: &str,
        _v: u64,
        _i: &str,
        _t: i64,
    ) -> Result<(), StripeError> {
        Ok(())
    }
    async fn meter_event_summary(
        &self,
        _m: &str,
        _c: &str,
        _s: i64,
        _e: i64,
    ) -> Result<u64, StripeError> {
        Ok(0)
    }
    async fn create_connect_account(
        &self,
        _e: &str,
        _c: &str,
        _co: &str,
    ) -> Result<String, StripeError> {
        Ok("acct_fake".into())
    }
    async fn create_account_link(
        &self,
        _a: &str,
        _r: &str,
        _rt: &str,
    ) -> Result<String, StripeError> {
        Ok("https://fake/onboard".into())
    }
    async fn retrieve_account(
        &self,
        id: &str,
    ) -> Result<zeroship_control::stripe_client::ConnectAccount, StripeError> {
        Ok(zeroship_control::stripe_client::ConnectAccount {
            id: id.into(),
            charges_enabled: true,
            payouts_enabled: true,
            details_submitted: true,
            creator_id: None,
        })
    }
    async fn create_connect_payment_intent(
        &self,
        _a: &str,
        _amt: u64,
        _cur: &str,
        _fee: u64,
        _d: &str,
        _k: &str,
    ) -> Result<zeroship_control::stripe_client::ConnectPaymentIntent, StripeError> {
        Ok(zeroship_control::stripe_client::ConnectPaymentIntent {
            id: "pi_fake".into(),
            client_secret: None,
        })
    }
    async fn invoice_settlement_ids(
        &self,
        _provider_invoice_id: &str,
    ) -> Result<(Option<String>, Option<String>), StripeError> {
        Ok((None, None))
    }
    async fn create_refund(
        &self,
        _provider_invoice_id: &str,
        _amount_cents: u64,
        _currency: &str,
        _idempotency_key: &str,
    ) -> Result<String, StripeError> {
        Ok(Self::unique("re"))
    }
}

// ===========================================================================
// Fixture (real PG; recording-fake Stripe).
// ===========================================================================

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
    zeroship_control::bootstrap_console::seed_plans(&registry)
        .await
        .expect("seed built-in plans");
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
    let control_pg = Arc::new(control_pg_client);

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
        control_pg,
        hydra_admin_url: "http://127.0.0.1:9".to_string(),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(zeroship_control::token_handlers::PatIssuer::dev_insecure()),
        auth_provider: zeroship_control::hydra_auth_provider("http://127.0.0.1:9"),
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

// ---------------------------------------------------------------------------
// DB seeding helpers (mirror billing_reconcile_test).
// ---------------------------------------------------------------------------

async fn make_user(state: &AppState, label: &str) -> Uuid {
    let email = format!("{label}-{}@example.test", Uuid::new_v4().simple());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, $2) RETURNING id",
            &[&email, &"Credit Creator".to_string()],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

async fn ensure_creator_billing(state: &AppState, creator: Uuid) {
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) \
             ON CONFLICT (creator_id) DO NOTHING",
            &[&creator],
        )
        .await
        .expect("ensure creator_billing");
}

/// A plan charging 1 cent/request, no included CU (fx = 1 cent/CU).
async fn make_plan(state: &AppState) -> String {
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("upsert requests weight");
    let plan_id = format!("pln_credit_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'credit-test', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id, &fx_one_cent],
        )
        .await
        .expect("seed priced plan");
    plan_id
}

async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("credit-{}", Uuid::new_v4());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&name, &plan_id, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app");
    let app_id: Uuid = rows[0].get("id");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&app_id, &owner],
        )
        .await
        .expect("insert owner membership");
    app_id
}

fn report(worker: &str, seq: u64, app: Uuid, requests: u64) -> UsageReport {
    let mut counters = std::collections::HashMap::new();
    counters.insert(app, AppUsage { requests, ..Default::default() });
    UsageReport {
        worker_id: worker.to_string(),
        report_id: Uuid::now_v7(),
        sequence: seq,
        counters,
    }
}

async fn ingest_at(state: &AppState, app: Uuid, requests: u64, period_start: i64, seq: u64) {
    let metering = Metering::new(state.registry.clone());
    let worker = format!("w-{}", Uuid::new_v4());
    metering
        .ingest_at(&report(&worker, seq, app, requests), period_start)
        .await
        .expect("ingest usage");
}

fn now_for_closed_period() -> i64 {
    chrono::Utc::now().timestamp()
}

fn prev_period(now: i64) -> i64 {
    billing_reconcile::previous_period_start_unix(now)
}

fn period_d(period_start: i64) -> chrono::NaiveDate {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc.timestamp_opt(period_start, 0).single().unwrap();
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).unwrap()
}

/// Read `(status, subtotal, credit, total)` for `(creator, period)`.
async fn read_invoice_money(
    state: &AppState,
    creator: Uuid,
    period_start: i64,
) -> Option<(String, i64, i64, i64)> {
    state
        .control_pg
        .query(
            "SELECT status, subtotal_cents, credit_cents, total_cents \
             FROM zeroship.invoices WHERE creator_id = $1 AND period = $2::date \
               AND status <> 'void'",
            &[&creator, &period_d(period_start)],
        )
        .await
        .expect("read invoice")
        .first()
        .map(|r| {
            (
                r.get::<_, String>("status"),
                r.get::<_, i64>("subtotal_cents"),
                r.get::<_, i64>("credit_cents"),
                r.get::<_, i64>("total_cents"),
            )
        })
}

/// The active invoice id for `(creator, period)`.
async fn invoice_id_for(state: &AppState, creator: Uuid, period_start: i64) -> String {
    state
        .control_pg
        .query(
            "SELECT id FROM zeroship.invoices \
             WHERE creator_id = $1 AND period = $2::date AND status <> 'void'",
            &[&creator, &period_d(period_start)],
        )
        .await
        .expect("read invoice id")[0]
        .get("id")
}

/// Insert a grant directly (bypassing the endpoint) so consume tests can control
/// created_at ordering / expiry. Returns the `crd_…` id.
async fn insert_grant(
    state: &AppState,
    creator: Uuid,
    amount: i64,
    currency: &str,
    created_at: chrono::DateTime<chrono::Utc>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    let id = zeroship_core::typed_id::new_credit_id();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.credit_ledger \
               (id, creator_id, kind, amount_cents, currency, expires_at, created_at) \
             VALUES ($1, $2, 'grant', $3, $4, $5, $6)",
            &[&id, &creator, &amount, &currency, &expires_at, &created_at],
        )
        .await
        .expect("insert grant");
    id
}

// ===========================================================================
// (a) credit_ledger is append-only; kind↔sign CHECK rejects bad signs.
// ===========================================================================

#[compio::test]
async fn credit_ledger_is_append_only_and_kind_sign_checked() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "appendonly").await;
    let creator = make_user(&fx.state, "appendonly").await;
    ensure_creator_billing(&fx.state, creator).await;

    let now = chrono::Utc::now();
    let grant_id = insert_grant(&fx.state, creator, 1000, "usd", now, None).await;

    // UPDATE is rejected by the immutability trigger.
    let upd = fx
        .state
        .control_pg
        .execute(
            "UPDATE zeroship.credit_ledger SET amount_cents = 1 WHERE id = $1",
            &[&grant_id],
        )
        .await;
    assert!(upd.is_err(), "credit_ledger UPDATE must be rejected by the immutability trigger");

    // DELETE is rejected too.
    let del = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.credit_ledger WHERE id = $1", &[&grant_id])
        .await;
    assert!(del.is_err(), "credit_ledger DELETE must be rejected by the immutability trigger");

    // kind↔sign CHECK: a POSITIVE 'consumed' is rejected.
    let bad_consumed = fx
        .state
        .control_pg
        .execute(
            "INSERT INTO zeroship.credit_ledger \
               (id, creator_id, kind, amount_cents, consumed_from_grant_id) \
             VALUES ($1, $2, 'consumed', 500, $3)",
            &[&zeroship_core::typed_id::new_credit_id(), &creator, &grant_id],
        )
        .await;
    assert!(bad_consumed.is_err(), "a POSITIVE 'consumed' entry must be rejected by the kind↔sign CHECK");

    // kind↔sign CHECK: a NEGATIVE 'grant' is rejected.
    let bad_grant = fx
        .state
        .control_pg
        .execute(
            "INSERT INTO zeroship.credit_ledger (id, creator_id, kind, amount_cents) \
             VALUES ($1, $2, 'grant', -500)",
            &[&zeroship_core::typed_id::new_credit_id(), &creator],
        )
        .await;
    assert!(bad_grant.is_err(), "a NEGATIVE 'grant' entry must be rejected by the kind↔sign CHECK");

    // The grant row survives the rejected mutations unchanged.
    let amt: i64 = fx
        .state
        .control_pg
        .query("SELECT amount_cents FROM zeroship.credit_ledger WHERE id = $1", &[&grant_id])
        .await
        .expect("read back")[0]
        .get("amount_cents");
    assert_eq!(amt, 1000);
}

// ===========================================================================
// (b) grant → finalize consumes oldest-first, writes credit_cents, appends
//     per-grant consumed entries; total = subtotal − credit + tax (tax=0).
// ===========================================================================

#[compio::test]
async fn finalize_consumes_oldest_first_and_balances() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "consume").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "consume").await;
    ensure_creator_billing(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // Two grants: $3 (older) + $5 (newer) = $8 available.
    let t0 = chrono::Utc::now() - chrono::Duration::hours(2);
    let t1 = chrono::Utc::now() - chrono::Duration::hours(1);
    let g_old = insert_grant(&fx.state, creator, 300, "usd", t0, None).await;
    let g_new = insert_grant(&fx.state, creator, 500, "usd", t1, None).await;

    // Bill $5 of usage (500 requests @ 1c). Credit $5 drawn entirely from g_old
    // ($3) then g_new ($2). total = 500 − 500 + 0 = 0.
    ingest_at(&fx.state, app, 500, period, 1).await;

    let billed = billing_reconcile::tick_with(&fx.state, &RecordingStripe::default(), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1);

    let inv = read_invoice_money(&fx.state, creator, period).await.expect("invoice");
    assert_eq!(inv.0, "finalized");
    assert_eq!(inv.1, 500, "subtotal");
    assert_eq!(inv.2, 500, "credit_cents = applied credit");
    assert_eq!(inv.3, 0, "total = subtotal − credit + tax (0) = 0");

    // Balance conserved: $8 granted − $5 consumed = $3.
    let bal = credit::balance(&*fx.state.control_pg, &creator, "usd").await.expect("balance");
    assert_eq!(bal, 300, "balance after consume = $8 − $5 = $3");

    // Per-grant consumed entries: g_old fully drawn (−300), g_new partially (−200).
    let inv_id = invoice_id_for(&fx.state, creator, period).await;
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT consumed_from_grant_id, amount_cents FROM zeroship.credit_ledger \
             WHERE applied_invoice_id = $1 AND kind = 'consumed' \
             ORDER BY created_at, id",
            &[&inv_id],
        )
        .await
        .expect("consumed rows");
    assert_eq!(rows.len(), 2, "one consumed entry PER drawn grant (g_old + g_new)");
    let drawn_old: i64 = rows
        .iter()
        .find(|r| r.get::<_, String>("consumed_from_grant_id") == g_old)
        .expect("g_old drawn")
        .get("amount_cents");
    let drawn_new: i64 = rows
        .iter()
        .find(|r| r.get::<_, String>("consumed_from_grant_id") == g_new)
        .expect("g_new drawn")
        .get("amount_cents");
    assert_eq!(drawn_old, -300, "oldest grant fully consumed first");
    assert_eq!(drawn_new, -200, "newer grant consumed for the remainder");
}

// ===========================================================================
// (c) a reconcile RE-RUN of the same period does NOT double-consume.
// ===========================================================================

#[compio::test]
async fn reconcile_rerun_does_not_double_consume() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "rerun").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "rerun").await;
    ensure_creator_billing(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    insert_grant(&fx.state, creator, 1000, "usd", chrono::Utc::now(), None).await;
    ingest_at(&fx.state, app, 400, period, 1).await; // $4 usage

    let billed1 = billing_reconcile::tick_with(&fx.state, &RecordingStripe::default(), now)
        .await
        .expect("tick 1");
    assert_eq!(billed1, 1);
    let bal1 = credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap();
    assert_eq!(bal1, 600, "after first run: $10 − $4 = $6");

    // RE-RUN the same period. The finalized short-circuit returns Ok(false); the
    // balance MUST be conserved (no second draw).
    let billed2 = billing_reconcile::tick_with(&fx.state, &RecordingStripe::default(), now)
        .await
        .expect("tick 2");
    assert_eq!(billed2, 0, "re-run is a no-op (already finalized)");
    let bal2 = credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap();
    assert_eq!(bal2, 600, "balance conserved across re-run — NO double-consume");

    // Exactly one consumed entry (one grant drawn once).
    let inv_id = invoice_id_for(&fx.state, creator, period).await;
    let n: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger \
             WHERE applied_invoice_id = $1 AND kind = 'consumed'",
            &[&inv_id],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(n, 1, "exactly one consumed entry — never doubled by a re-run");
}

// ===========================================================================
// (c') the consume_at_finalize helper is itself idempotent on a draft re-drive
//      (the draft-claim path, not just the finalized short-circuit). Drives the
//      REAL helper twice on the SAME draft invoice id inside a tx.
// ===========================================================================

#[compio::test]
async fn consume_helper_is_idempotent_on_draft_redrive() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "draftredrive").await;
    let creator = make_user(&fx.state, "draftredrive").await;
    ensure_creator_billing(&fx.state, creator).await;
    insert_grant(&fx.state, creator, 1000, "usd", chrono::Utc::now(), None).await;

    // Claim a DRAFT invoice (the stable per-(creator,period) anchor).
    let inv = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv, &creator, &period_d(prev_period(now_for_closed_period()))],
        )
        .await
        .expect("claim draft");

    // First consume: draw $6 of a $6 subtotal from the $10 grant.
    let first = credit::consume_at_finalize(&*fx.state.control_pg, &creator, &inv, 600, "usd")
        .await
        .expect("consume 1");
    assert_eq!(first.applied_cents, 600);
    assert_eq!(first.draws.len(), 1, "one consumed entry on first pass");

    // SECOND consume on the SAME draft id (a crash-window re-drive): recompute,
    // append NOTHING.
    let second = credit::consume_at_finalize(&*fx.state.control_pg, &creator, &inv, 600, "usd")
        .await
        .expect("consume 2");
    assert_eq!(second.applied_cents, 600, "re-run recomputes the same applied credit");
    assert_eq!(second.draws.len(), 0, "re-run appends NO new consumed entry");

    // Balance conserved: $10 − $6 = $4 (not $10 − $12).
    let bal = credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap();
    assert_eq!(bal, 400, "balance conserved — the helper never double-draws on a re-drive");
}

// ===========================================================================
// (d) an expires_at-expired grant is NOT consumed.
// ===========================================================================

#[compio::test]
async fn expired_grant_is_not_consumed() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "expired").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "expired").await;
    ensure_creator_billing(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // An EXPIRED $10 grant (expired an hour ago) + a LIVE $2 grant.
    let past = chrono::Utc::now() - chrono::Duration::hours(1);
    insert_grant(&fx.state, creator, 1000, "usd", past - chrono::Duration::hours(1), Some(past)).await;
    insert_grant(&fx.state, creator, 200, "usd", chrono::Utc::now(), None).await;

    ingest_at(&fx.state, app, 500, period, 1).await; // $5 usage

    let billed = billing_reconcile::tick_with(&fx.state, &RecordingStripe::default(), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1);

    // Only the $2 LIVE grant is consumable: credit = min($2, $5) = $2; total = $3.
    let inv = read_invoice_money(&fx.state, creator, period).await.expect("invoice");
    assert_eq!(inv.2, 200, "only the non-expired $2 grant is drawn (the expired $10 is skipped)");
    assert_eq!(inv.3, 300, "total = 500 − 200 = 300");
}

// ===========================================================================
// (f) a non-USD grant is NOT drawn against a USD bill (currency filter).
// ===========================================================================

#[compio::test]
async fn non_usd_grant_is_not_drawn_against_usd_bill() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "currency").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "currency").await;
    ensure_creator_billing(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // A stray EUR grant (directly inserted) + a USD grant.
    insert_grant(&fx.state, creator, 1000, "eur", chrono::Utc::now() - chrono::Duration::hours(2), None).await;
    insert_grant(&fx.state, creator, 100, "usd", chrono::Utc::now(), None).await;

    ingest_at(&fx.state, app, 500, period, 1).await; // $5 usage

    let billed = billing_reconcile::tick_with(&fx.state, &RecordingStripe::default(), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1);

    // Only the $1 USD grant is drawn — the EUR grant is invisible to the USD bill.
    let inv = read_invoice_money(&fx.state, creator, period).await.expect("invoice");
    assert_eq!(inv.2, 100, "only the USD grant is drawn against the USD bill");
    assert_eq!(inv.3, 400, "total = 500 − 100 = 400");

    // The USD balance reflects only USD entries; the EUR grant is separate.
    let usd_bal = credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap();
    assert_eq!(usd_bal, 0, "USD balance: $1 granted − $1 consumed = 0");
    let eur_bal = credit::balance(&*fx.state.control_pg, &creator, "eur").await.unwrap();
    assert_eq!(eur_bal, 1000, "the EUR grant is untouched");
}

// ===========================================================================
// grant() helper: idempotency-key reuse — same body returns first, different
// body is a Conflict (no second grant).
// ===========================================================================

#[compio::test]
async fn grant_helper_idempotency_key_and_fingerprint() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "granthelper").await;
    let creator = make_user(&fx.state, "granthelper").await;
    ensure_creator_billing(&fx.state, creator).await;

    let key = format!("idem-{}", Uuid::new_v4());

    // First grant.
    let r1 = credit::grant(&*fx.state.control_pg, &creator, 500, "usd", "grant", None, None, &key)
        .await
        .expect("grant 1");
    let id1 = match r1 {
        GrantOutcome::Created(id) => id,
        other => panic!("expected Created, got {other:?}"),
    };

    // Same key + SAME body ⇒ Duplicate (the first grant id), no second row.
    let r2 = credit::grant(&*fx.state.control_pg, &creator, 500, "usd", "grant", None, None, &key)
        .await
        .expect("grant 2");
    assert_eq!(r2, GrantOutcome::Duplicate(id1.clone()), "same key+body returns the first grant");

    // Same key + DIFFERENT body ⇒ Conflict (no second grant).
    let r3 = credit::grant(&*fx.state.control_pg, &creator, 999, "usd", "grant", None, None, &key)
        .await
        .expect("grant 3");
    assert_eq!(r3, GrantOutcome::Conflict, "same key + different amount is a conflict");

    // Exactly ONE grant row for this key.
    let n: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger WHERE idempotency_key = $1",
            &[&key],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(n, 1, "exactly one grant for the reused key — no double grant");

    // A non-USD grant is rejected at the boundary.
    let bad = credit::grant(
        &*fx.state.control_pg,
        &creator,
        500,
        "eur",
        "grant",
        None,
        None,
        &format!("idem-{}", Uuid::new_v4()),
    )
    .await;
    assert!(bad.is_err(), "a non-USD grant is rejected at the Rust boundary (v1 USD-pinned)");
}

// ===========================================================================
// (e) the operator endpoint: 403 for a creator token; 409 on idem-key reuse with
//     a different body (no double grant). Drives the REAL `api::grant_credit`
//     handler through an ntex test app + the REAL authz guard.
// ===========================================================================

struct Pat {
    token: String,
}

impl Pat {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

/// Issue a real PAT bound to `policy` (faithful AuthzGuard path).
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
             VALUES ($1, $2, 'pat', 'credit PAT', $3, $4, $5)",
            &[&token_id, &user_id, &policies, &hash, &expires_at],
        )
        .await
        .expect("insert PAT row");
    Pat { token }
}

/// Operator policy: BillingWrite on `Resource::Any` — the fleet-wide gate.
fn billing_any() -> Policy {
    Policy {
        name: "operator".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![AuthzAction::BillingRead, AuthzAction::BillingWrite],
            resources: vec![AuthzResource::Any],
            conditions: Vec::new(),
        }],
    }
}

/// Creator self policy: BillingWrite on an OWN app only — NOT `Resource::Any`, so
/// the operator-only credit endpoint denies it (the 403 path).
fn billing_self() -> Policy {
    Policy {
        name: "creator self".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![AuthzAction::BillingRead, AuthzAction::BillingWrite],
            resources: vec![AuthzResource::App { id: Uuid::new_v4().to_string() }],
            conditions: Vec::new(),
        }],
    }
}

#[compio::test]
async fn grant_endpoint_operator_only_and_idempotency_conflict() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "endpoint").await;
    let creator = make_user(&fx.state, "endpoint").await;
    ensure_creator_billing(&fx.state, creator).await;

    // A creator-self PAT (App-scoped BillingWrite — NOT Resource::Any) and an
    // operator PAT (Resource::Any BillingWrite).
    let creator_user = make_user(&fx.state, "creator-token").await;
    let creator_pat = issue_pat(&fx.state, creator_user, None, billing_self()).await;
    let op_user = make_user(&fx.state, "operator").await;
    let op_pat = issue_pat(&fx.state, op_user, Some("billing"), billing_any()).await;

    let app = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/billing/credit")
                .route(web::post().to(zeroship_control::api::grant_credit)),
        ),
    )
    .await;
    let key = format!("idem-{}", Uuid::new_v4());
    let body = serde_json::json!({"creator_id": creator, "amount_cents": 500});

    // (1) A CREATOR token (App-scoped only) is 403 — credit is never self-grantable.
    let req = test::TestRequest::post()
        .uri("/api/billing/credit")
        .header("idempotency-key", key.as_str())
        .header("authorization", creator_pat.bearer())
        .set_json(&body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a creator (App-scoped) token must be 403 on the operator-only credit endpoint",
    );

    // (2) Operator grant succeeds (201 Created).
    let req = test::TestRequest::post()
        .uri("/api/billing/credit")
        .header("idempotency-key", key.as_str())
        .header("authorization", op_pat.bearer())
        .set_json(&body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::CREATED, "operator grant is 201");

    // (3) Same key + SAME body ⇒ 200 (idempotent retry, no second grant).
    let req = test::TestRequest::post()
        .uri("/api/billing/credit")
        .header("idempotency-key", key.as_str())
        .header("authorization", op_pat.bearer())
        .set_json(&body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "same key+body is an idempotent 200 retry");

    // (4) Same key + DIFFERENT body ⇒ 409 (no double grant).
    let body2 = serde_json::json!({"creator_id": creator, "amount_cents": 999});
    let req = test::TestRequest::post()
        .uri("/api/billing/credit")
        .header("idempotency-key", key.as_str())
        .header("authorization", op_pat.bearer())
        .set_json(&body2)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "same key + different body is a 409");

    // Exactly ONE grant for the key, amount 500.
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT amount_cents FROM zeroship.credit_ledger WHERE idempotency_key = $1",
            &[&key],
        )
        .await
        .expect("count");
    assert_eq!(rows.len(), 1, "exactly one grant for the reused key");
    assert_eq!(rows[0].get::<_, i64>("amount_cents"), 500, "the first body won; no second grant");

    // (5) Missing Idempotency-Key header ⇒ 400.
    let req = test::TestRequest::post()
        .uri("/api/billing/credit")
        .header("authorization", op_pat.bearer())
        .set_json(&body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "missing Idempotency-Key is a 400");
}

// ===========================================================================
// MINOR-2: `note` IS part of the grant fingerprint. A reused key with a DIFFERENT
// note is a body change ⇒ 409, never a silent return of the first grant.
// (RED pre-fix: `grant_fingerprint` excluded `note`, so the second call returned
//  Duplicate(first) and the assertion `== Conflict` failed.)
// ===========================================================================

#[compio::test]
async fn grant_note_change_is_a_conflict() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "notefp").await;
    let creator = make_user(&fx.state, "notefp").await;
    ensure_creator_billing(&fx.state, creator).await;

    let key = format!("idem-{}", Uuid::new_v4());

    // First grant carries note "promo A".
    let r1 = credit::grant(
        &*fx.state.control_pg, &creator, 500, "usd", "grant", None, Some("promo A"), &key,
    )
    .await
    .expect("grant 1");
    let id1 = match r1 {
        GrantOutcome::Created(id) => id,
        other => panic!("expected Created, got {other:?}"),
    };

    // Same key + same body INCLUDING the note ⇒ Duplicate (safe retry).
    let r_same = credit::grant(
        &*fx.state.control_pg, &creator, 500, "usd", "grant", None, Some("promo A"), &key,
    )
    .await
    .expect("grant same");
    assert_eq!(
        r_same,
        GrantOutcome::Duplicate(id1.clone()),
        "same key + identical body (note included) is a safe-retry Duplicate",
    );

    // Same key + DIFFERENT note ⇒ Conflict (note is in the fingerprint).
    let r_diff = credit::grant(
        &*fx.state.control_pg, &creator, 500, "usd", "grant", None, Some("promo B"), &key,
    )
    .await
    .expect("grant diff note");
    assert_eq!(
        r_diff,
        GrantOutcome::Conflict,
        "a reused key with a CHANGED note is a 409 conflict (note is fingerprinted)",
    );

    // Some("") vs None must also be distinguishable (presence byte).
    let key2 = format!("idem-{}", Uuid::new_v4());
    let none_grant = credit::grant(
        &*fx.state.control_pg, &creator, 100, "usd", "grant", None, None, &key2,
    )
    .await
    .expect("none note");
    assert!(matches!(none_grant, GrantOutcome::Created(_)));
    let empty_note = credit::grant(
        &*fx.state.control_pg, &creator, 100, "usd", "grant", None, Some(""), &key2,
    )
    .await
    .expect("empty note");
    assert_eq!(
        empty_note,
        GrantOutcome::Conflict,
        "Some(\"\") differs from None in the fingerprint — a body change ⇒ conflict",
    );

    // Exactly ONE grant row for `key` — no second grant ever appended.
    let n: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger WHERE idempotency_key = $1",
            &[&key],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(n, 1, "the note conflict never created a second grant");
}

// ===========================================================================
// MINOR-3: the idempotency conflict re-SELECT is creator-scoped. A key reused
// ACROSS creators is a Conflict for the second creator (never another creator's
// grant id) and appends no grant for them.
// NOTE on RED: this finding is DEFENSE-IN-DEPTH — it cannot produce a behavioral
// RED against the pre-fix helper, because `creator_id` is ALREADY part of
// `grant_fingerprint`. With the old unscoped `WHERE idempotency_key = $1`, creator
// B's reuse read A's row, computed B's (different) fingerprint, and returned
// Conflict anyway. The scoped re-SELECT makes that explicit (B's `else` branch
// returns Conflict without ever touching A's row) and forecloses any future
// fingerprint scheme that drops creator_id. The assertions below hold under both,
// so this test is a correctness GUARD, not a RED-distinguishing regression.
// ===========================================================================

#[compio::test]
async fn grant_idempotency_key_is_creator_scoped() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "xtenant").await;
    let creator_a = make_user(&fx.state, "xtenant-a").await;
    let creator_b = make_user(&fx.state, "xtenant-b").await;
    ensure_creator_billing(&fx.state, creator_a).await;
    ensure_creator_billing(&fx.state, creator_b).await;

    // Both creators share the SAME idempotency key (cross-tenant reuse).
    let key = format!("idem-shared-{}", Uuid::new_v4());

    let r_a = credit::grant(
        &*fx.state.control_pg, &creator_a, 500, "usd", "grant", None, None, &key,
    )
    .await
    .expect("grant a");
    let id_a = match r_a {
        GrantOutcome::Created(id) => id,
        other => panic!("expected Created for creator A, got {other:?}"),
    };

    // Creator B reuses A's key. The globally-unique index makes the INSERT no-op;
    // the creator-scoped re-SELECT finds no row for B ⇒ Conflict (NOT A's id).
    let r_b = credit::grant(
        &*fx.state.control_pg, &creator_b, 500, "usd", "grant", None, None, &key,
    )
    .await
    .expect("grant b");
    assert_eq!(
        r_b,
        GrantOutcome::Conflict,
        "a cross-tenant key reuse is a Conflict for creator B, never creator A's grant id",
    );
    assert_ne!(
        r_b,
        GrantOutcome::Duplicate(id_a.clone()),
        "creator B must NEVER receive creator A's grant id",
    );

    // No credit_ledger row for creator B with that key.
    let n_b: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger \
             WHERE idempotency_key = $1 AND creator_id = $2",
            &[&key, &creator_b],
        )
        .await
        .expect("count b")[0]
        .get("n");
    assert_eq!(n_b, 0, "no grant was appended for creator B");
    // Creator A's grant is intact.
    let bal_a = credit::balance(&*fx.state.control_pg, &creator_a, "usd").await.unwrap();
    assert_eq!(bal_a, 500, "creator A's grant is untouched");
    let bal_b = credit::balance(&*fx.state.control_pg, &creator_b, "usd").await.unwrap();
    assert_eq!(bal_b, 0, "creator B has no credit");
}

// ===========================================================================
// MINOR-6: `kind` is normalized case-insensitively (uniform with `currency`).
// `GRANT` / `Promo` are accepted and stored lowercase; the domain CHECK is
// lowercase-only, so a non-normalized kind would have FK/domain-violated.
// (RED pre-fix: `matches!(kind, "grant"|...)` was case-sensitive ⇒ "GRANT" was
//  rejected with InvalidInput before it ever reached the INSERT.)
// ===========================================================================

#[compio::test]
async fn grant_kind_is_case_insensitive() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "kindcase").await;
    let creator = make_user(&fx.state, "kindcase").await;
    ensure_creator_billing(&fx.state, creator).await;

    let key = format!("idem-{}", Uuid::new_v4());
    let r = credit::grant(
        &*fx.state.control_pg, &creator, 700, "usd", "GRANT", None, None, &key,
    )
    .await
    .expect("uppercase kind accepted");
    let id = match r {
        GrantOutcome::Created(id) => id,
        other => panic!("expected Created, got {other:?}"),
    };

    // Stored lowercase (the domain CHECK is lowercase-only).
    let kind: String = fx
        .state
        .control_pg
        .query("SELECT kind FROM zeroship.credit_ledger WHERE id = $1", &[&id])
        .await
        .expect("read kind")[0]
        .get("kind");
    assert_eq!(kind, "grant", "an uppercase kind is normalized to lowercase before INSERT");

    // A mixed-case "Promo" is also accepted.
    let r2 = credit::grant(
        &*fx.state.control_pg,
        &creator,
        100,
        "usd",
        "Promo",
        None,
        None,
        &format!("idem-{}", Uuid::new_v4()),
    )
    .await
    .expect("mixed-case promo accepted");
    assert!(matches!(r2, GrantOutcome::Created(_)));
}

// ===========================================================================
// MINOR-5 (+ MINOR-4): the grant endpoint classifies the `creator_billing` INSERT
// failure by SQLSTATE — only a foreign_key_violation (23503, a non-existent user) is
// a 400 "unknown creator". A grant for a NON-EXISTENT creator_id (no users row) ⇒
// 400; a grant for a real creator ⇒ 201 — both through the REAL one-`transaction()`
// upsert+grant path (MINOR-4). NOTE on RED: the ghost→400 outcome matches the pre-fix
// string-match behaviour (the pre-fix code 400'd ANY error), so this is not a
// RED-distinguishing test for the mis-classification per se — faithfully injecting a
// transient/non-FK error against real PG mid-INSERT is impractical. It GUARDS that the
// FK→400 path and the real→201 path both still hold under the SQLSTATE classifier and
// the single transaction (a non-FK error now routes to 500 via `error_response`).
// ===========================================================================

#[compio::test]
async fn grant_endpoint_unknown_creator_is_fk_400() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "fk400").await;
    let op_user = make_user(&fx.state, "operator-fk").await;
    let op_pat = issue_pat(&fx.state, op_user, Some("billing"), billing_any()).await;

    let app = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/billing/credit")
                .route(web::post().to(zeroship_control::api::grant_credit)),
        ),
    )
    .await;

    // A creator_id with NO `users` row ⇒ the creator_billing FK violates ⇒ 400.
    let ghost = Uuid::new_v4();
    let body = serde_json::json!({"creator_id": ghost, "amount_cents": 500});
    let req = test::TestRequest::post()
        .uri("/api/billing/credit")
        .header("idempotency-key", format!("idem-{}", Uuid::new_v4()))
        .header("authorization", op_pat.bearer())
        .set_json(&body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a grant for a non-existent creator is a 400 (FK violation classified by SQLSTATE 23503)",
    );

    // A grant for a REAL creator still succeeds (201) through the same path.
    let real = make_user(&fx.state, "real-creator").await;
    let body_ok = serde_json::json!({"creator_id": real, "amount_cents": 500});
    let req = test::TestRequest::post()
        .uri("/api/billing/credit")
        .header("idempotency-key", format!("idem-{}", Uuid::new_v4()))
        .header("authorization", op_pat.bearer())
        .set_json(&body_ok)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::CREATED, "a grant for a real creator is 201");
}

// ===========================================================================
// MAJOR-1: `consume_at_finalize` self-serializes per creator via a
// transaction-scoped advisory lock. Two assertions:
//   (1) while a tx holds the consume lock for a creator, a SECOND connection's
//       `pg_try_advisory_xact_lock` on the SAME key fails (the lock is held);
//       a DIFFERENT creator's key still succeeds (per-creator, not global).
//   (2) sequential over-draw is impossible: two consumes against ONE grant draw
//       at most the grant balance, and the balance never goes negative.
// (RED pre-fix: no lock was taken in `consume_at_finalize`, so assertion (1)'s
//  try-lock on the same key would SUCCEED even mid-consume-tx.)
// ===========================================================================

#[compio::test]
async fn consume_takes_per_creator_advisory_lock() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "lock").await;
    let creator = make_user(&fx.state, "lock").await;
    let other = make_user(&fx.state, "lock-other").await;
    ensure_creator_billing(&fx.state, creator).await;
    ensure_creator_billing(&fx.state, other).await;
    insert_grant(&fx.state, creator, 1000, "usd", chrono::Utc::now(), None).await;

    // A draft invoice anchor for the consume.
    let inv = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv, &creator, &period_d(prev_period(now_for_closed_period()))],
        )
        .await
        .expect("claim draft");

    // A SECOND independent connection used as the lock observer.
    let (obs_client, obs_conn) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("observer connect");
    compio::runtime::spawn(async move {
        let _ = obs_conn.run().await;
    })
    .detach();

    // Open a transaction on a DEDICATED connection and run consume inside it; the
    // transaction-scoped advisory lock is held until we commit/rollback.
    let (mut conn, conn_run) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("consume connect");
    compio::runtime::spawn(async move {
        let _ = conn_run.run().await;
    })
    .detach();
    let tx = conn.transaction().await.expect("tx");
    let applied = credit::consume_at_finalize(&tx, &creator, &inv, 600, "usd")
        .await
        .expect("consume");
    assert_eq!(applied.applied_cents, 600, "drew $6 of the $10 grant");

    // (1) While the consume tx is OPEN (lock held), a try-lock on the SAME key from
    //     the observer connection must FAIL.
    let key_held: bool = obs_client
        .query(
            "SELECT pg_try_advisory_xact_lock(hashtext($1::text)::bigint) AS got",
            &[&creator.to_string()],
        )
        .await
        .expect("try-lock held")[0]
        .get("got");
    assert!(
        !key_held,
        "the consume tx holds the per-creator advisory lock — a concurrent try-lock must fail",
    );

    // A DIFFERENT creator's key is free (the lock is per-creator, not global).
    let key_other: bool = obs_client
        .query(
            "SELECT pg_try_advisory_xact_lock(hashtext($1::text)::bigint) AS got",
            &[&other.to_string()],
        )
        .await
        .expect("try-lock other")[0]
        .get("got");
    assert!(
        key_other,
        "a DIFFERENT creator's advisory lock is free — the lock serializes per creator only",
    );
    // The observer's own try-lock (creator=other) is xact-scoped to ITS implicit
    // txn; release it explicitly so it can't leak into other tests on this conn.
    obs_client
        .execute("SELECT pg_advisory_unlock_all()", &[])
        .await
        .ok();

    // Commit the consume; the lock releases at commit.
    tx.commit().await.expect("commit consume");

    // (2) A SECOND consume on a fresh invoice draws at most the REMAINING balance —
    //     never over-drawing the grant. Remaining is $4; ask for $9, get $4.
    let inv2 = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[
                &inv2,
                &creator,
                &period_d(prev_period(now_for_closed_period() - 86_400 * 40)),
            ],
        )
        .await
        .expect("claim draft 2");
    let (mut conn2, conn2_run) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("consume2 connect");
    compio::runtime::spawn(async move {
        let _ = conn2_run.run().await;
    })
    .detach();
    let tx2 = conn2.transaction().await.expect("tx2");
    let applied2 = credit::consume_at_finalize(&tx2, &creator, &inv2, 900, "usd")
        .await
        .expect("consume 2");
    tx2.commit().await.expect("commit 2");
    assert_eq!(
        applied2.applied_cents, 400,
        "the second consume draws only the remaining $4 — the grant is never over-drawn",
    );

    // Balance is exactly 0 (never negative): $10 − $6 − $4 = $0.
    let bal = credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap();
    assert_eq!(bal, 0, "balance is non-negative and exact after both draws ($10 − $6 − $4)");
    assert!(bal >= 0, "balance MUST never go negative");
}

// ===========================================================================
// #16: consume against an EMPTY ledger (no grants at all) ⇒ `applied_cents == 0` and
//      ZERO `consumed` rows appended. The reconciler then finalizes total = subtotal.
// ===========================================================================

#[compio::test]
async fn consume_with_empty_ledger_applies_zero_and_appends_nothing() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "emptyledger").await;
    let creator = make_user(&fx.state, "emptyledger").await;
    ensure_creator_billing(&fx.state, creator).await;
    // NO grants for this creator.

    // A draft invoice anchor.
    let inv = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv, &creator, &period_d(prev_period(now_for_closed_period()))],
        )
        .await
        .expect("claim draft");

    // Consume against a $5 subtotal with NO available credit.
    let applied = credit::consume_at_finalize(&*fx.state.control_pg, &creator, &inv, 500, "usd")
        .await
        .expect("consume on empty ledger");
    assert_eq!(applied.applied_cents, 0, "no credit available ⇒ applied = 0");
    assert!(applied.draws.is_empty(), "no draws on an empty ledger");

    // ZERO consumed rows for this invoice.
    let n: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger \
             WHERE applied_invoice_id = $1 AND kind = 'consumed'",
            &[&inv],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(n, 0, "an empty-ledger consume appends NO consumed entries");
    // Balance stays exactly 0 (nothing granted, nothing drawn).
    assert_eq!(credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap(), 0);
}

// ===========================================================================
// #15: a ZERO-subtotal consume short-circuits — `consume_at_finalize` returns
//      `applied_cents == 0` with NO draws even though a grant IS available (the
//      `subtotal_cents <= 0` guard fires before any draw). A grant must NOT be drawn
//      against a $0 bill.
// ===========================================================================

#[compio::test]
async fn consume_with_zero_subtotal_short_circuits() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "zerosub").await;
    let creator = make_user(&fx.state, "zerosub").await;
    ensure_creator_billing(&fx.state, creator).await;
    // A LIVE $10 grant is available.
    insert_grant(&fx.state, creator, 1000, "usd", chrono::Utc::now(), None).await;

    let inv = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv, &creator, &period_d(prev_period(now_for_closed_period()))],
        )
        .await
        .expect("claim draft");

    // Subtotal 0 ⇒ short-circuit (no draw against a $0 bill).
    let applied = credit::consume_at_finalize(&*fx.state.control_pg, &creator, &inv, 0, "usd")
        .await
        .expect("consume zero subtotal");
    assert_eq!(applied.applied_cents, 0, "a $0 subtotal draws no credit");
    assert!(applied.draws.is_empty(), "no draws on a $0 subtotal");

    let n: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger \
             WHERE applied_invoice_id = $1 AND kind = 'consumed'",
            &[&inv],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(n, 0, "a zero-subtotal consume appends NO consumed entries");
    // The $10 grant is untouched.
    assert_eq!(
        credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap(),
        1000, "the grant is preserved — never drawn against a $0 bill",
    );
}

// ===========================================================================
// #17: a grant that lands AFTER a consume is NOT drawn by that tick. We consume
//      against the grants visible at consume time, then append a LATE grant; the
//      already-consumed invoice's applied credit is unchanged (no retroactive draw),
//      and the late grant's full value is preserved in the balance.
//      (Models a grant landing concurrently with a consume: the consume reads its
//      grant set at draw time; a later grant is simply available for the NEXT bill.)
// ===========================================================================

#[compio::test]
async fn late_grant_is_not_drawn_by_an_earlier_consume() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "lategrant").await;
    let creator = make_user(&fx.state, "lategrant").await;
    ensure_creator_billing(&fx.state, creator).await;
    // An initial $3 grant.
    insert_grant(&fx.state, creator, 300, "usd", chrono::Utc::now() - chrono::Duration::hours(1), None).await;

    let inv = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv, &creator, &period_d(prev_period(now_for_closed_period()))],
        )
        .await
        .expect("claim draft");

    // Consume against a $5 subtotal: only the $3 grant is visible ⇒ applied = $3.
    let applied = credit::consume_at_finalize(&*fx.state.control_pg, &creator, &inv, 500, "usd")
        .await
        .expect("consume");
    assert_eq!(applied.applied_cents, 300, "only the $3 grant visible at consume time is drawn");
    assert_eq!(applied.draws.len(), 1, "one consumed entry (the $3 grant)");

    // A LATE $10 grant lands AFTER the consume.
    let late = insert_grant(&fx.state, creator, 1000, "usd", chrono::Utc::now(), None).await;

    // The already-consumed invoice's applied credit is UNCHANGED (no retroactive draw):
    // a re-drive of the SAME invoice recomputes $3 (the re-run guard recomputes from the
    // existing consumed rows; it does NOT draw the late grant).
    let redrive = credit::consume_at_finalize(&*fx.state.control_pg, &creator, &inv, 500, "usd")
        .await
        .expect("re-drive consume");
    assert_eq!(redrive.applied_cents, 300, "re-drive recomputes $3 — the late grant is NOT retroactively drawn");
    assert!(redrive.draws.is_empty(), "re-drive appends nothing");

    // No consumed entry was EVER drawn from the late grant.
    let drawn_from_late: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger \
             WHERE kind = 'consumed' AND consumed_from_grant_id = $1",
            &[&late],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(drawn_from_late, 0, "the late grant was never drawn by the earlier consume");

    // Balance: $3 granted − $3 consumed + $10 late grant = $10 (the late grant is fully
    // available for the NEXT bill — no over-draw, the consume never touched it).
    assert_eq!(
        credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap(),
        1000, "the late grant's full value is preserved for the next bill",
    );
}

// ===========================================================================
// #5: the `grant` Rust boundary rejects NON-OPERATOR kinds. `consumed`,
//     `void_reversal`, and `refund_clawback` are reconciler-internal (negative-sign /
//     grant-ref-bound) and MUST NOT be operator-grantable — `grant()` returns
//     `InvalidInput` before any INSERT.
// ===========================================================================

#[compio::test]
async fn grant_rejects_non_operator_kinds_at_the_boundary() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "kindreject").await;
    let creator = make_user(&fx.state, "kindreject").await;
    ensure_creator_billing(&fx.state, creator).await;

    for kind in ["consumed", "void_reversal", "refund_clawback"] {
        let r = credit::grant(
            &*fx.state.control_pg,
            &creator,
            500,
            "usd",
            kind,
            None,
            None,
            &format!("idem-{}", Uuid::new_v4()),
        )
        .await;
        assert!(
            matches!(r, Err(RegistryError::InvalidInput(_))),
            "grant kind {kind:?} must be rejected at the Rust boundary, got {r:?}",
        );
    }

    // NO ledger row was appended for any of the rejected kinds.
    let n: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(n, 0, "a rejected-kind grant appends NO ledger row");
}

// ===========================================================================
// #30: the `credit_ledger_grant_ref` CHECK (0048/0054) binds the grant-ref column to
//      the entry kind: a NEGATIVE/consuming kind (consumed/void_reversal/refund_clawback)
//      MUST carry `consumed_from_grant_id`; a positive grant-class kind MUST NOT. Two
//      illegal rows are rejected by the CHECK:
//        * a `consumed` row with NULL `consumed_from_grant_id`;
//        * a `grant` row with a NON-NULL `consumed_from_grant_id`.
//      RED if the `credit_ledger_grant_ref` CHECK is dropped.
// ===========================================================================

#[compio::test]
async fn credit_ledger_grant_ref_check_binds_kind_to_grant_reference() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "grantref").await;
    let creator = make_user(&fx.state, "grantref").await;
    ensure_creator_billing(&fx.state, creator).await;
    // A real grant to reference (so the FK on consumed_from_grant_id can resolve when
    // we test the positive-with-ref rejection).
    let grant_id = insert_grant(&fx.state, creator, 1000, "usd", chrono::Utc::now(), None).await;

    // (1) A `consumed` row with NULL consumed_from_grant_id is rejected (a consume MUST
    //     name the grant it drew). amount is negative (the kind↔sign CHECK requires it).
    let bad_consumed = fx
        .state
        .control_pg
        .execute(
            "INSERT INTO zeroship.credit_ledger \
               (id, creator_id, kind, amount_cents, currency) \
             VALUES ($1, $2, 'consumed', -100, 'usd')",
            &[&zeroship_core::typed_id::new_credit_id(), &creator],
        )
        .await;
    assert!(
        bad_consumed.is_err(),
        "a 'consumed' entry with NULL consumed_from_grant_id must be rejected by the grant-ref CHECK",
    );

    // (2) A `grant` row WITH a non-NULL consumed_from_grant_id is rejected (a grant
    //     references no prior grant). amount is positive (grant sign).
    let bad_grant = fx
        .state
        .control_pg
        .execute(
            "INSERT INTO zeroship.credit_ledger \
               (id, creator_id, kind, amount_cents, currency, consumed_from_grant_id) \
             VALUES ($1, $2, 'grant', 100, 'usd', $3)",
            &[&zeroship_core::typed_id::new_credit_id(), &creator, &grant_id],
        )
        .await;
    assert!(
        bad_grant.is_err(),
        "a 'grant' entry with a non-NULL consumed_from_grant_id must be rejected by the grant-ref CHECK",
    );

    // Only the legitimate seed grant survives.
    let n: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(n, 1, "neither illegal row was inserted — only the seed grant exists");
}

// ===========================================================================
// #13: credit is CAPPED at the subtotal — a SINGLE grant far larger than the bill draws
//      only `min(grant, subtotal)`. `applied == subtotal`, `total == 0`, and the leftover
//      grant balance is preserved (one partial `consumed` entry against that one grant).
// ===========================================================================

#[compio::test]
async fn single_large_grant_is_capped_at_subtotal_leftover_preserved() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "creditcap").await;
    let creator = make_user(&fx.state, "creditcap").await;
    ensure_creator_billing(&fx.state, creator).await;
    // A SINGLE $100 grant — far larger than the $6 bill.
    let big = insert_grant(&fx.state, creator, 10_000, "usd", chrono::Utc::now(), None).await;

    let inv = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv, &creator, &period_d(prev_period(now_for_closed_period()))],
        )
        .await
        .expect("claim draft");

    // Consume against a $6 subtotal: applied = min($100, $6) = $6.
    let applied = credit::consume_at_finalize(&*fx.state.control_pg, &creator, &inv, 600, "usd")
        .await
        .expect("consume");
    assert_eq!(applied.applied_cents, 600, "credit capped at the subtotal ($6), NOT the full $100 grant");
    assert_eq!(applied.draws.len(), 1, "one partial draw against the single grant");
    assert_eq!(applied.draws[0].grant_id, big, "the draw is against the big grant");
    assert_eq!(applied.draws[0].amount_cents, 600, "exactly the subtotal was drawn");

    // Leftover balance preserved: $100 − $6 = $94.
    assert_eq!(
        credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap(),
        9400, "the leftover grant balance ($94) is preserved",
    );

    // Exactly ONE consumed entry against the one grant; magnitude $6.
    let drawn: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COALESCE(SUM(amount_cents),0)::bigint AS s FROM zeroship.credit_ledger \
             WHERE applied_invoice_id = $1 AND kind = 'consumed'",
            &[&inv],
        )
        .await
        .expect("sum")[0]
        .get("s");
    assert_eq!(drawn, -600, "one consumed entry of −$6 (the capped draw)");
}

// ===========================================================================
// #33: the per-creator finalize lock serializes `consume_at_finalize` against a
//      concurrent `record_plan_change_tx` for the SAME creator — they take the IDENTICAL
//      `pg_advisory_xact_lock(hashtext(creator)::bigint)` key, so a plan change can never
//      interleave a consume to over-draw. We hold the lock via a consume on an OPEN tx,
//      prove a concurrent `record_plan_change_tx` BLOCKS on the same key, then release and
//      assert the balance is exact + non-negative after both committed.
//
//      RED-proof: PINS that the consume op takes the per-creator lock (the lock "moved
//      into the consume op to make non-negativity intrinsic"). If `consume_at_finalize`
//      dropped its `pg_advisory_xact_lock`, the spawned `record_plan_change_tx` would NOT
//      block on the creator key — the "must be WAITING" assertion would fail.
// ===========================================================================

#[compio::test]
async fn consume_and_record_plan_change_serialize_on_the_creator_lock() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "consume-vs-planchange").await;
    let creator = make_user(&fx.state, "cvp").await;
    ensure_creator_billing(&fx.state, creator).await;
    let plan_a = make_plan(&fx.state).await;
    let plan_b = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan_a, creator).await;
    insert_grant(&fx.state, creator, 1000, "usd", chrono::Utc::now(), None).await;

    // A draft invoice anchor for the consume.
    let inv = zeroship_core::typed_id::new_invoice_id();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv, &creator, &period_d(prev_period(now_for_closed_period()))],
        )
        .await
        .expect("claim draft");

    // The advisory key the creator lock hashes to.
    let lock_key: i64 = fx
        .state
        .control_pg
        .query("SELECT hashtext($1::text)::bigint AS k", &[&creator.to_string()])
        .await
        .expect("hash key")[0]
        .get("k");

    // (1) Run `consume_at_finalize` inside an OPEN tx on a dedicated connection. It takes
    //     the per-creator advisory lock as its first act and HOLDS it until we commit.
    let (mut conn, conn_run) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("consume connect");
    compio::runtime::spawn(async move {
        let _ = conn_run.run().await;
    })
    .detach();
    let tx = conn.transaction().await.expect("tx");
    let applied = credit::consume_at_finalize(&tx, &creator, &inv, 600, "usd")
        .await
        .expect("consume drew $6 of the $10 grant");
    assert_eq!(applied.applied_cents, 600);

    // (2) SPAWN a concurrent `record_plan_change_tx` for the SAME creator. It must BLOCK
    //     trying to acquire the SAME per-creator advisory key (proving they serialize).
    let registry = fx.state.registry.clone();
    let app_w = app;
    let creator_w = creator;
    let from_w = plan_a.clone();
    let to_w = plan_b.clone();
    let now_unix = now_for_closed_period();
    let task = compio::runtime::spawn(async move {
        zeroship_control::proration::record_plan_change_tx(
            &registry, &app_w, &creator_w, Some(&from_w), &to_w, now_unix,
        )
        .await
    });

    // Poll pg_locks until the spawned plan-change is provably WAITING on the creator key.
    let mut waiting = false;
    for _ in 0..200 {
        let n: i64 = fx
            .state
            .control_pg
            .query(
                "SELECT COUNT(*)::bigint AS n FROM pg_locks \
                 WHERE locktype = 'advisory' AND NOT granted \
                   AND ((classid::bigint << 32) | objid::bigint) = $1",
                &[&lock_key],
            )
            .await
            .expect("poll pg_locks")[0]
            .get("n");
        if n >= 1 {
            waiting = true;
            break;
        }
        compio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        waiting,
        "a concurrent record_plan_change_tx must BLOCK on the SAME per-creator advisory key the \
         consume holds — if it never waits, the two ops do not serialize (over-draw window)",
    );

    // (3) Commit the consume → release the lock. The plan-change now acquires it + commits.
    tx.commit().await.expect("commit consume");
    let outcome = task.await.expect("plan-change task did not panic").expect("plan-change commits");
    assert!(
        matches!(outcome, zeroship_control::proration::PlanChangeOutcome::Recorded { .. }),
        "the plan change recorded once the lock freed, got {outcome:?}",
    );

    // Balance is exact + non-negative: $10 granted − $6 consumed = $4 (the plan change
    // never touched credit; serialization prevented any over-draw).
    let bal = credit::balance(&*fx.state.control_pg, &creator, "usd").await.unwrap();
    assert_eq!(bal, 400, "balance after the serialized consume = $10 − $6 = $4");
    assert!(bal >= 0, "balance never goes negative under consume↔plan-change serialization");
}
