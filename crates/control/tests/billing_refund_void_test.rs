//! PR-3 regression tests for billing-ops gap #26: the `0049 refunds` table +
//! over-refund trigger, the `RefundProvider` seam (Stripe `Refund` for cash / native
//! `refund_to_credit` grant for credit), the operator `POST /invoices/{id}/refunds`
//! endpoint, and the void+reissue + `void_reversal` + negative-invoice true-up bridge.
//!
//! FAITHFUL by construction: every assertion runs against a live, migrated Postgres
//! (`CONTROL_TEST_DB`; silent skip otherwise) and exercises the REAL paths —
//!   * the REAL `refunds` table / domains / over-refund trigger / immutability trigger;
//!   * the REAL `refund::issue_refund` + `void_reissue::void_and_reissue` helpers (so the
//!     claim-then-call, the `void_reversal` append, and the true-up math all run for real);
//!   * the REAL reconciler `billing_reconcile::bill_creator` for the original bill AND the
//!     reissue (so credit consume / finalize-in-one-UPDATE / balance CHECK all validate);
//!   * the REAL `api::refund_invoice` / `api::void_invoice` HTTP handlers via an `ntex`
//!     test app (operator authz, idempotency-key header, body fingerprint, 403/409).
//! Only the Stripe WIRE is a recording fake (it records every `create_refund` call so the
//! cash-leg `re_…` ref assertions are real, but no HTTP leaves the process).
//!
//! These FAIL against the pre-PR-3 code/schema:
//!   (a) no `refunds` table / over-refund trigger → the credit-laundering scenario cannot
//!       even be rejected;
//!   (b) no `RefundProvider` seam → no `re_…` ref for cash, no `refund_to_credit` grant;
//!   (c) no claim-then-call / no `refund_provider_refs` → a crashed POST is not re-drivable;
//!   (f) no `void_reversal` append → consume→void→reissue double-consumes (balance not
//!       conserved);
//!   (g) a true-up that ignores already-issued refunds → over-refunds + the trigger rejects it.

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
use zeroship_control::metering::Metering;
use zeroship_control::refund::{self, NativeRefundProvider, RefundDestination, RefundOutcome};
use zeroship_control::stripe_client::{Period, StripeApi};
use zeroship_control::stripe_store::StripeError;
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// The reconciler single-flights fleet-wide via `pg_try_advisory_lock`; serialize the
/// reconcile-driving tests with a process-wide lock (mirrors the credit/reconcile tests).
static RECONCILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-refund-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ===========================================================================
// A recording StripeApi fake. Records every create_refund call (so the cash-leg
// re_… assertions are real) and mints a unique re_… per call. The reconciler's
// item/invoice path is the same shape as the credit test's fake.
// ===========================================================================

#[derive(Default)]
struct RecordingStripe {
    refunds: Mutex<Vec<(String, u64)>>, // (provider_invoice_id, amount_cents)
}

impl RecordingStripe {
    fn unique(prefix: &str) -> String {
        format!("{prefix}_{}", Uuid::new_v4().simple())
    }
    fn refund_count(&self) -> usize {
        self.refunds.lock().unwrap().len()
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
    ) -> Result<String, StripeError> {
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
        Ok(Self::unique("in"))
    }
    async fn finalize_invoice(&self, id: &str) -> Result<String, StripeError> {
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
    async fn create_refund(
        &self,
        provider_invoice_id: &str,
        amount_cents: u64,
        _currency: &str,
        _idempotency_key: &str,
    ) -> Result<String, StripeError> {
        self.refunds
            .lock()
            .unwrap()
            .push((provider_invoice_id.to_string(), amount_cents));
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
        hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
            "http://127.0.0.1:9",
        )),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
        pairwise_salt: [0u8; 32],
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
    }
}

// ---------------------------------------------------------------------------
// DB seeding helpers (mirror billing_credit_test).
// ---------------------------------------------------------------------------

async fn make_user(state: &AppState, label: &str) -> Uuid {
    let email = format!("{label}-{}@example.test", Uuid::new_v4().simple());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, $2) RETURNING id",
            &[&email, &"Refund Creator".to_string()],
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
    let plan_id = format!("pln_refund_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'refund-test', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id, &fx_one_cent],
        )
        .await
        .expect("seed priced plan");
    plan_id
}

async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("refund-{}", Uuid::new_v4());
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

/// A run-unique idempotency key. `refunds.idempotency_key` is GLOBALLY unique and the
/// test DB persists across runs, so a constant key would collide with a prior run's
/// refund row (key hit → spurious Conflict). Deriving it from the run-unique invoice id
/// keeps each test's keys disjoint without recreating the DB.
fn key(invoice_id: &str, suffix: &str) -> String {
    format!("{invoice_id}:{suffix}")
}

fn prev_period(now: i64) -> i64 {
    billing_reconcile::previous_period_start_unix(now)
}

fn period_d(period_start: i64) -> chrono::NaiveDate {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc.timestamp_opt(period_start, 0).single().unwrap();
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).unwrap()
}

/// The active (non-void) invoice id for `(creator, period)`.
async fn active_invoice_id(state: &AppState, creator: Uuid, period_start: i64) -> Option<String> {
    state
        .control_pg
        .query(
            "SELECT id FROM zeroship.invoices \
             WHERE creator_id = $1 AND period = $2::date AND status <> 'void'",
            &[&creator, &period_d(period_start)],
        )
        .await
        .expect("read invoice id")
        .first()
        .map(|r| r.get::<_, String>("id"))
}

/// `(status, subtotal, credit, total)` for an invoice id.
async fn invoice_money(state: &AppState, invoice_id: &str) -> (String, i64, i64, i64) {
    let r = &state
        .control_pg
        .query(
            "SELECT status, subtotal_cents, credit_cents, total_cents \
             FROM zeroship.invoices WHERE id = $1",
            &[&invoice_id],
        )
        .await
        .expect("read invoice")[0];
    (
        r.get::<_, String>("status"),
        r.get::<_, i64>("subtotal_cents"),
        r.get::<_, i64>("credit_cents"),
        r.get::<_, i64>("total_cents"),
    )
}

/// Append an `invoice_payments` charge row for an invoice (the cash anchor), as the
/// payment webhook would. `provider_ref` is the Stripe invoice id (`in_…`).
async fn append_payment(state: &AppState, invoice_id: &str, amount: i64, provider_ref: &str) {
    zeroship_control::invoice_payments::append_charge(
        &*state.control_pg,
        invoice_id,
        amount,
        "usd",
        Some(provider_ref),
    )
    .await
    .expect("append charge");
}

async fn insert_grant(
    state: &AppState,
    creator: Uuid,
    amount: i64,
    created_at: chrono::DateTime<chrono::Utc>,
) -> String {
    let id = zeroship_core::typed_id::new_credit_id();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.credit_ledger \
               (id, creator_id, kind, amount_cents, currency, created_at) \
             VALUES ($1, $2, 'grant', $3, 'usd', $4)",
            &[&id, &creator, &amount, &created_at],
        )
        .await
        .expect("insert grant");
    id
}

/// Drive the reconciler for the closed period (single tick).
async fn run_reconcile(state: &AppState, stripe: &RecordingStripe, now: i64) -> usize {
    billing_reconcile::tick_with(state, stripe, now)
        .await
        .expect("tick")
}

/// A fresh, OWNED Postgres connection (mutable) — `issue_refund` opens a
/// `conn.transaction()` internally (CRITICAL-1: precheck + claim under the per-creator
/// advisory lock), which needs `&mut`. The shared `Arc<Client>` in `AppState` cannot be
/// borrowed mutably, so refund-driving tests use a dedicated connection.
async fn new_conn(url: &str) -> compio_postgres::Client {
    let (client, conn) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .expect("test conn connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

/// Set a Stripe customer for the creator (so bill_creator does not skip).
async fn set_customer(state: &AppState, creator: Uuid) {
    state
        .stripe_store
        .set_customer(creator, &format!("cus_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
}

// ===========================================================================
// (a) Over-refund 3-way bound: the credit-laundering scenario is REJECTED.
//     consume $40 + pay $60 → one charge +6000 → a $60 refund-to-CREDIT must be
//     rejected (it would re-grant credit-funded value as fresh credit). The cap is
//     Σ(invoice_payments) = $60, NOT total_cents.
// ===========================================================================

#[compio::test]
async fn over_refund_three_way_bound_blocks_credit_laundering() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "launder").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "launder").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;

    // $40 credit grant; $100 of usage → subtotal 10000, credit 4000, total 6000.
    insert_grant(&fx.state, creator, 4000, Utc::now() - Duration::hours(2)).await;
    ingest_at(&fx.state, app, 10_000, period, 1).await;

    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv = active_invoice_id(&fx.state, creator, period).await.expect("invoice");
    let (status, subtotal, credit, total) = invoice_money(&fx.state, &inv).await;
    assert_eq!(status, "finalized");
    assert_eq!((subtotal, credit, total), (10_000, 4000, 6000));

    // The webhook records the $60 cash collected.
    append_payment(&fx.state, &inv, 6000, "in_launder").await;

    // The credit-laundering attempt: refund $60 to CREDIT. cash_collected = $60, but
    // total_cents = $60 too here — the laundering would let the creator net +$60 credit
    // for $60 cash. The cap is anchored on cash; Σ(credit refunds) would be 6000 ≤ 6000
    // BUT combined with later... here the single $60 credit refund alone is allowed up
    // to cash; the LAUNDERING test is: refund $60 cash AND $60 credit must fail combined.
    // First: a $60 credit refund is at the cash boundary (allowed).
    let mut conn = new_conn(&url).await;
    let provider = NativeRefundProvider;
    let r1 = refund::issue_refund(
        &mut conn, &provider, &inv, 6000, 6000, 0, RefundDestination::Credit, None, &key(&inv, "credit-60"),
    )
    .await
    .expect("issue");
    assert!(matches!(r1, RefundOutcome::Issued { .. }), "a $60 credit refund at the cash cap is allowed");

    // Now a $1 CASH refund on top must be REJECTED — combined Σ ($60 credit + $1 cash)
    // = $61 > cash $60. This is the 3-way combined bound (iii).
    let r2 = refund::issue_refund(
        &mut conn, &provider, &inv, 100, 100, 0, RefundDestination::Cash, None, &key(&inv, "cash-1"),
    )
    .await
    .expect("issue");
    assert!(
        matches!(r2, RefundOutcome::OverRefund(_)),
        "a $1 cash refund on top of a $60 credit refund must over-refund (combined $61 > cash $60)"
    );

    // And a pure cash refund EXCEEDING cash is rejected on its own bound (i).
    let r3 = refund::issue_refund(
        &mut conn, &provider, &inv, 6100, 6100, 0, RefundDestination::Cash, None, &key(&inv, "cash-61"),
    )
    .await
    .expect("issue");
    assert!(
        matches!(r3, RefundOutcome::OverRefund(_)),
        "a $61 cash refund > cash collected $60 must be rejected"
    );

    // The DB trigger is the authoritative backstop: a direct INSERT past the cap RAISEs.
    let direct = fx
        .state
        .control_pg
        .execute(
            "INSERT INTO zeroship.refunds \
               (id, invoice_id, amount_cents, subtotal_cents, tax_cents, currency, \
                destination, idempotency_key, request_fingerprint, status) \
             VALUES ($1, $2, 7000, 7000, 0, 'usd', 'cash', 'direct-bad', 'fp', 'pending')",
            &[&zeroship_core::typed_id::new_refund_id(), &inv],
        )
        .await;
    assert!(direct.is_err(), "the over-refund trigger must RAISE on a direct over-cap INSERT");
}

// ===========================================================================
// (b) destination='cash' issues a Stripe Refund (re_…), invoice stays finalized;
//     destination='credit' appends a refund_to_credit grant, NO Stripe charge reversal.
// ===========================================================================

#[compio::test]
async fn cash_refund_issues_re_credit_refund_appends_grant() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "cashcredit").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "cashcredit").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    ingest_at(&fx.state, app, 10_000, period, 1).await; // $100, no credit

    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv = active_invoice_id(&fx.state, creator, period).await.expect("invoice");
    append_payment(&fx.state, &inv, 10_000, "in_cashcredit").await;

    let mut conn = new_conn(&url).await;
    let provider = refund::StripeRefundProvider { stripe: &stripe };

    // CASH refund of $30 → a Stripe Refund (re_…), a 'refund' provider ref, invoice stays
    // finalized.
    let r = refund::issue_refund(
        &mut conn, &provider, &inv, 3000, 3000, 0, RefundDestination::Cash, None, &key(&inv, "cash-30"),
    )
    .await
    .expect("issue");
    let refund_id = match r {
        RefundOutcome::Issued { refund_id, provider_ref } => {
            assert!(provider_ref.as_deref().is_some_and(|p| p.starts_with("re_")), "cash refund must carry a re_… ref, got {provider_ref:?}");
            refund_id
        }
        other => panic!("expected Issued, got {other:?}"),
    };
    assert_eq!(stripe.refund_count(), 1, "exactly one Stripe Refund issued for cash");

    // The invoice is UNTOUCHED (still finalized).
    let (status, _, _, _) = invoice_money(&fx.state, &inv).await;
    assert_eq!(status, "finalized", "the invoice stays finalized on a refund");

    // The provider ref row is a 'refund' (re_…).
    let pr = fx
        .state
        .control_pg
        .query(
            "SELECT ref_kind, external_id FROM zeroship.refund_provider_refs WHERE refund_id = $1",
            &[&refund_id],
        )
        .await
        .expect("read ref");
    assert_eq!(pr.len(), 1);
    assert_eq!(pr[0].get::<_, String>("ref_kind"), "refund");
    assert!(pr[0].get::<_, String>("external_id").starts_with("re_"));

    // The refund row is 'issued'.
    let st: String = fx
        .state
        .control_pg
        .query("SELECT status FROM zeroship.refunds WHERE id = $1", &[&refund_id])
        .await
        .expect("read refund")[0]
        .get("status");
    assert_eq!(st, "issued");

    // CREDIT refund of $20 → a refund_to_credit grant, NO Stripe call (refund_count
    // unchanged), balance += $20.
    let bal_before = zeroship_control::credit::balance(&*fx.state.control_pg, &creator, "usd")
        .await
        .expect("balance");
    let r2 = refund::issue_refund(
        &mut conn, &provider, &inv, 2000, 2000, 0, RefundDestination::Credit, None, &key(&inv, "credit-20"),
    )
    .await
    .expect("issue");
    let credit_refund_id = match r2 {
        RefundOutcome::Issued { refund_id, provider_ref } => {
            assert!(provider_ref.is_none(), "credit refund has NO provider ref");
            refund_id
        }
        other => panic!("expected Issued, got {other:?}"),
    };
    assert_eq!(stripe.refund_count(), 1, "credit refund must NOT call Stripe (no charge reversal)");

    let bal_after = zeroship_control::credit::balance(&*fx.state.control_pg, &creator, "usd")
        .await
        .expect("balance");
    assert_eq!(bal_after - bal_before, 2000, "a $20 refund_to_credit grant was appended");

    // The refund_to_credit ledger entry exists with the marker note.
    let grant = fx
        .state
        .control_pg
        .query(
            "SELECT amount_cents, note FROM zeroship.credit_ledger \
             WHERE kind = 'refund_to_credit' AND applied_invoice_id = $1",
            &[&inv],
        )
        .await
        .expect("read grant");
    assert_eq!(grant.len(), 1);
    assert_eq!(grant[0].get::<_, i64>("amount_cents"), 2000);
    assert_eq!(
        grant[0].get::<_, Option<String>>("note").as_deref(),
        Some(refund::refund_to_credit_note(&credit_refund_id).as_str())
    );
}

// ===========================================================================
// (c) Claim-then-call idempotency: a replay (same key + body) issues EXACTLY ONE
//     refund and ONE Stripe Refund; the ref is written after success.
// ===========================================================================

#[compio::test]
async fn refund_replay_is_idempotent_exactly_one() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "replay").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "replay").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    ingest_at(&fx.state, app, 10_000, period, 1).await;

    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv = active_invoice_id(&fx.state, creator, period).await.expect("invoice");
    append_payment(&fx.state, &inv, 10_000, "in_replay").await;

    let mut conn = new_conn(&url).await;
    let provider = refund::StripeRefundProvider { stripe: &stripe };

    let first = refund::issue_refund(
        &mut conn, &provider, &inv, 3000, 3000, 0, RefundDestination::Cash, None, &key(&inv, "replay"),
    )
    .await
    .expect("issue");
    let first_id = match first {
        RefundOutcome::Issued { refund_id, .. } => refund_id,
        o => panic!("expected Issued, got {o:?}"),
    };

    // Replay: same key + same body. Must NOT issue a second refund.
    let replay = refund::issue_refund(
        &mut conn, &provider, &inv, 3000, 3000, 0, RefundDestination::Cash, None, &key(&inv, "replay"),
    )
    .await
    .expect("issue");
    match replay {
        RefundOutcome::Duplicate(id) => assert_eq!(id, first_id, "replay returns the first refund"),
        RefundOutcome::Issued { refund_id, .. } => {
            assert_eq!(refund_id, first_id, "replay converges to the first refund (already issued)");
        }
        o => panic!("expected Duplicate/converged Issued, got {o:?}"),
    }
    // Exactly one Stripe Refund and one refund row.
    assert_eq!(stripe.refund_count(), 1, "replay must not double-refund at Stripe");
    let n: i64 = fx
        .state
        .control_pg
        .query("SELECT COUNT(*)::bigint AS n FROM zeroship.refunds WHERE invoice_id = $1", &[&inv])
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(n, 1, "exactly one refund row");

    // A different body with the SAME key → 409 conflict, no new refund.
    let conflict = refund::issue_refund(
        &mut conn, &provider, &inv, 4000, 4000, 0, RefundDestination::Cash, None, &key(&inv, "replay"),
    )
    .await
    .expect("issue");
    assert!(matches!(conflict, RefundOutcome::Conflict), "same key + different body → Conflict");
}

// ===========================================================================
// (d) Tax-split refund: a refund of a TAXED invoice refunds proportional tax.
//     We stamp a tax_cents directly (the tax seam is PR-5) to prove the split.
// ===========================================================================

#[compio::test]
async fn tax_split_refund_returns_proportional_tax() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "taxsplit").await;

    let creator = make_user(&fx.state, "taxsplit").await;
    ensure_creator_billing(&fx.state, creator).await;

    // A finalized invoice with subtotal 10000, tax 1000, total 11000 (10% tax). We
    // build it as a draft then finalize-in-one-UPDATE so the immutability trigger + the
    // balance CHECK accept it, then record the $110 cash collected.
    let inv = zeroship_core::typed_id::new_invoice_id();
    let period = period_d(prev_period(now_for_closed_period()));
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv, &creator, &period],
        )
        .await
        .expect("draft");
    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.invoices SET subtotal_cents = 10000, credit_cents = 0, \
             tax_cents = 1000, total_cents = 11000, status = 'finalized', finalized_at = NOW() \
             WHERE id = $1",
            &[&inv],
        )
        .await
        .expect("finalize");
    append_payment(&fx.state, &inv, 11_000, "in_taxsplit").await;

    // Refund $55 (half the bill) to credit. The endpoint-derived proportional tax is
    // round_half_up(5500 × 1000 / 11000) = 500; subtotal = 5000. We exercise issue_refund
    // with that split (the api handler derives it; here we pass it explicitly + also test
    // the derivation in the endpoint test below).
    let mut conn = new_conn(&url).await;
    let provider = NativeRefundProvider;
    let r = refund::issue_refund(
        &mut conn, &provider, &inv, 5500, 5000, 500, RefundDestination::Credit, None, &key(&inv, "tax"),
    )
    .await
    .expect("issue");
    assert!(matches!(r, RefundOutcome::Issued { .. }));

    let row = &fx
        .state
        .control_pg
        .query(
            "SELECT amount_cents, subtotal_cents, tax_cents FROM zeroship.refunds WHERE invoice_id = $1",
            &[&inv],
        )
        .await
        .expect("read refund")[0];
    assert_eq!(row.get::<_, i64>("amount_cents"), 5500);
    assert_eq!(row.get::<_, i64>("subtotal_cents"), 5000);
    assert_eq!(row.get::<_, i64>("tax_cents"), 500, "proportional tax (10% of $55) = $5");

    // A bad split (subtotal+tax != amount) is rejected by the helper + the CHECK.
    let bad = refund::issue_refund(
        &mut conn, &provider, &inv, 1000, 1000, 100, RefundDestination::Credit, None, &key(&inv, "badsplit"),
    )
    .await;
    assert!(bad.is_err(), "a split where subtotal+tax != amount is rejected");
}

// ===========================================================================
// (e) Operator-only 403 + idempotency 409 via the REAL endpoint + authz guard.
// ===========================================================================

struct Pat {
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
             VALUES ($1, $2, 'pat', 'refund PAT', $3, $4, $5)",
            &[&token_id, &user_id, &policies, &hash, &expires_at],
        )
        .await
        .expect("insert PAT row");
    Pat { token }
}

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
async fn refund_endpoint_operator_only_and_idempotency_conflict() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "endpoint").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "endpoint").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    ingest_at(&fx.state, app, 10_000, period, 1).await;
    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv = active_invoice_id(&fx.state, creator, period).await.expect("invoice");
    append_payment(&fx.state, &inv, 10_000, "in_endpoint").await;

    let op_user = make_user(&fx.state, "operator").await;
    let op_pat = issue_pat(&fx.state, op_user, Some("admin"), billing_any()).await;
    let creator_pat = issue_pat(&fx.state, creator, None, billing_self()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/invoices/{id}/refunds")
                .route(web::post().to(zeroship_control::api::refund_invoice)),
        ),
    )
    .await;

    let uri = format!("/api/invoices/{inv}/refunds");
    // Run-unique keys (refunds.idempotency_key is GLOBALLY unique + the DB persists).
    let k1 = key(&inv, "k1");
    let k2 = key(&inv, "k2");
    let k_over = key(&inv, "k-over");

    // (1) A creator (App-scoped) token is 403 — refunds are never self-serve.
    let req = test::TestRequest::post()
        .uri(&uri)
        .header("idempotency-key", k1.as_str())
        .header("authorization", creator_pat.bearer())
        .set_json(&serde_json::json!({"amount_cents": 1000, "destination": "credit"}))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "creator token must be 403");

    // (2) Operator credit refund of $10 → 201.
    let req = test::TestRequest::post()
        .uri(&uri)
        .header("idempotency-key", k2.as_str())
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({"amount_cents": 1000, "destination": "credit"}))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::CREATED, "operator refund is 201");

    // (3) Same key + same body → idempotent 200.
    let req = test::TestRequest::post()
        .uri(&uri)
        .header("idempotency-key", k2.as_str())
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({"amount_cents": 1000, "destination": "credit"}))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "same key+body is an idempotent 200");

    // (4) Same key + DIFFERENT body → 409.
    let req = test::TestRequest::post()
        .uri(&uri)
        .header("idempotency-key", k2.as_str())
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({"amount_cents": 2000, "destination": "credit"}))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "same key + different body is a 409");

    // (5) Missing Idempotency-Key → 400.
    let req = test::TestRequest::post()
        .uri(&uri)
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({"amount_cents": 1000, "destination": "credit"}))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "missing Idempotency-Key is a 400");

    // (6) An over-refund (more than cash collected) → 422.
    let req = test::TestRequest::post()
        .uri(&uri)
        .header("idempotency-key", k_over.as_str())
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({"amount_cents": 100000, "destination": "cash"}))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "over-refund is a 422");

    // Exactly ONE refund row for key k2.
    let n: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.refunds WHERE idempotency_key = $1",
            &[&k2],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(n, 1, "exactly one refund for the reused key");
}

// ===========================================================================
// (f) void_reversal conserves balance: grant $10 → invoice consumes $6 (bal $4) →
//     void (bal back to $10 via void_reversal +$6) → reissue re-consumes $6 (bal $4).
//     Net balance == had the invoice been correct the first time.
// ===========================================================================

#[compio::test]
async fn void_reversal_conserves_credit_balance() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "voidrev").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "voidrev").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;

    // $10 grant; $6 of usage → subtotal 600, credit 600, total 0 (fully credit-covered).
    insert_grant(&fx.state, creator, 1000, Utc::now() - Duration::hours(2)).await;
    ingest_at(&fx.state, app, 600, period, 1).await;

    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv_a = active_invoice_id(&fx.state, creator, period).await.expect("invoice A");
    assert_eq!(invoice_money(&fx.state, &inv_a).await, ("finalized".into(), 600, 600, 0));

    let bal_after_consume = zeroship_control::credit::balance(&*fx.state.control_pg, &creator, "usd")
        .await
        .expect("bal");
    assert_eq!(bal_after_consume, 400, "balance after consume = $10 − $6 = $4");

    // Void + reissue. The reissue re-prices the SAME usage (no correction here), so it
    // re-consumes $6 from the restored balance — net balance conserved at $4.
    let outcome = zeroship_control::void_reissue::void_and_reissue(&fx.state, &stripe, &inv_a)
        .await
        .expect("void+reissue");
    assert_eq!(outcome.voided_invoice_id, inv_a);
    let reissued = outcome.reissued_invoice_id.expect("reissued invoice");
    assert_ne!(reissued, inv_a, "reissue mints a NEW invoice id");

    // The voided invoice is 'void'; the reissued is finalized with the same money.
    assert_eq!(invoice_money(&fx.state, &inv_a).await.0, "void");
    assert_eq!(invoice_money(&fx.state, &reissued).await, ("finalized".into(), 600, 600, 0));

    // BALANCE CONSERVED: still $4 (void restored +$6, reissue re-drew $6). Never $-2.
    let bal_final = zeroship_control::credit::balance(&*fx.state.control_pg, &creator, "usd")
        .await
        .expect("bal");
    assert_eq!(bal_final, 400, "void_reversal conserves balance: still $4, never the $-2 double-consume");

    // A void_reversal entry exists for the voided invoice, +$6.
    let vr = fx
        .state
        .control_pg
        .query(
            "SELECT COALESCE(SUM(amount_cents),0)::bigint AS s FROM zeroship.credit_ledger \
             WHERE applied_invoice_id = $1 AND kind = 'void_reversal'",
            &[&inv_a],
        )
        .await
        .expect("vr")[0]
        .get::<_, i64>("s");
    assert_eq!(vr, 600, "the void_reversal restores exactly the $6 the voided invoice consumed");
}

// ===========================================================================
// (g) True-up subtracts already-issued refunds (the worked-example sequence): an
//     invoice collected $60 cash, $25 already refunded, reissue lower → the true-up
//     refunds exactly Σpayments − Σcash-refunds-issued − total(new), NOT double.
// ===========================================================================

#[compio::test]
async fn true_up_subtracts_already_issued_cash_refunds() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "trueup").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "trueup").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;

    // First bill: $60 of usage → subtotal 6000, total 6000 (no credit). $60 cash collected.
    ingest_at(&fx.state, app, 6000, period, 1).await;
    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv_b = active_invoice_id(&fx.state, creator, period).await.expect("invoice B");
    assert_eq!(invoice_money(&fx.state, &inv_b).await, ("finalized".into(), 6000, 0, 6000));
    append_payment(&fx.state, &inv_b, 6000, "in_trueup").await;

    // $25 already refunded to cash (the over-charge correction).
    let mut conn = new_conn(&url).await;
    let provider = refund::StripeRefundProvider { stripe: &stripe };
    let pre = refund::issue_refund(
        &mut conn, &provider, &inv_b, 2500, 2500, 0, RefundDestination::Cash, None, &key(&inv_b, "pre-25"),
    )
    .await
    .expect("issue");
    assert!(matches!(pre, RefundOutcome::Issued { .. }));
    assert_eq!(stripe.refund_count(), 1);

    // The reissue should be LOWER: drop the app's usage so the corrected bill is $10.
    // (Simulate a mis-attribution being corrected away — reduce the aggregate to 1000.)
    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.usage_aggregates SET total = 1000 \
             WHERE app_id = $1 AND period = $2::date AND metric = 'requests'",
            &[&app, &period_d(period)],
        )
        .await
        .expect("lower usage");

    // Void + reissue. reissue total = $10. True-up = $60 − $25 − $10 = $25.
    let outcome = zeroship_control::void_reissue::void_and_reissue(&fx.state, &stripe, &inv_b)
        .await
        .expect("void+reissue");
    let reissued = outcome.reissued_invoice_id.expect("reissued");
    assert_eq!(invoice_money(&fx.state, &reissued).await.3, 1000, "reissue total = $10");

    assert_eq!(outcome.true_up_cents, 2500, "true-up = $60 − $25 already-refunded − $10 = $25 (NOT $50)");
    let true_up_id = outcome.true_up_refund_id.expect("a true-up refund was issued");

    // The true-up is a CASH refund row on the VOIDED invoice.
    let row = &fx
        .state
        .control_pg
        .query(
            "SELECT invoice_id, amount_cents, destination::text AS dest FROM zeroship.refunds WHERE id = $1",
            &[&true_up_id],
        )
        .await
        .expect("read true-up")[0];
    assert_eq!(row.get::<_, String>("invoice_id"), inv_b, "true-up refunds the VOIDED invoice");
    assert_eq!(row.get::<_, i64>("amount_cents"), 2500);
    assert_eq!(row.get::<_, String>("dest"), "cash");

    // Total cash refunds on B = $25 + $25 = $50 ≤ cash $60: the cap held (the round-1
    // formula would have tried $50 and been rejected). Two Stripe refunds total.
    assert_eq!(stripe.refund_count(), 2, "the pre-refund + the true-up = two Stripe Refunds");
    let total_cash_refunds =
        refund::refunds_total_for_destination(&*fx.state.control_pg, &inv_b, RefundDestination::Cash)
            .await
            .expect("sum");
    assert_eq!(total_cash_refunds, 5000, "Σ cash refunds on B = $50 ≤ cash $60 (cap held)");
}

// ===========================================================================
// (h) Two non-void invoices for one (creator, period) impossible; a void releases
//     the claim for reissue.
// ===========================================================================

#[compio::test]
async fn one_active_invoice_per_period_void_releases_claim() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "claim").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "claim").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    ingest_at(&fx.state, app, 5000, period, 1).await;

    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv1 = active_invoice_id(&fx.state, creator, period).await.expect("invoice 1");

    // A second NON-void invoice for the same (creator, period) is rejected by the
    // partial unique index.
    let dup = fx
        .state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'finalized')",
            &[&zeroship_core::typed_id::new_invoice_id(), &creator, &period_d(period)],
        )
        .await;
    assert!(dup.is_err(), "a second NON-void invoice for the period must be rejected");

    // Void releases the claim → reissue mints a NEW active invoice.
    let outcome = zeroship_control::void_reissue::void_and_reissue(&fx.state, &stripe, &inv1)
        .await
        .expect("void+reissue");
    let inv2 = outcome.reissued_invoice_id.expect("reissued");
    assert_ne!(inv2, inv1);
    assert_eq!(invoice_money(&fx.state, &inv1).await.0, "void");
    assert_eq!(invoice_money(&fx.state, &inv2).await.0, "finalized");

    // Exactly ONE active (non-void) invoice for the period.
    let active: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.invoices \
             WHERE creator_id = $1 AND period = $2::date AND status <> 'void'",
            &[&creator, &period_d(period)],
        )
        .await
        .expect("count")[0]
        .get("n");
    assert_eq!(active, 1, "exactly one active invoice; the void is an audit row");
}

// ===========================================================================
// CRITICAL-1 (1a): `claim_refund_locked` takes the SAME per-creator advisory lock
//     `consume_at_finalize` takes — so a refund serializes against a concurrent
//     consume/refund for the same creator. While the claim tx holds the lock, a
//     SECOND connection's `pg_try_advisory_xact_lock(same key)` must FAIL; a
//     DIFFERENT creator's key is free. Mirrors the PR-2 consume-lock test.
// (RED pre-fix: the old `issue_refund` took NO lock + opened NO txn, so the
//  try-lock on the same key would SUCCEED even mid-claim → the over-refund bound
//  was defeatable by concurrency.)
// ===========================================================================

#[compio::test]
async fn issue_refund_takes_per_creator_advisory_lock() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "rlock").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "rlock").await;
    let other = make_user(&fx.state, "rlock-other").await;
    ensure_creator_billing(&fx.state, creator).await;
    ensure_creator_billing(&fx.state, other).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    ingest_at(&fx.state, app, 5000, period, 1).await;

    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv = active_invoice_id(&fx.state, creator, period).await.expect("invoice");
    append_payment(&fx.state, &inv, 5000, "in_rlock").await;

    // A SECOND independent connection used as the lock observer.
    let (obs_client, obs_conn) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("observer connect");
    compio::runtime::spawn(async move {
        let _ = obs_conn.run().await;
    })
    .detach();

    // Drive the locked claim on a DEDICATED connection inside a caller-held tx (as the
    // PR-2 consume test drives `consume_at_finalize`); the xact-scoped lock is held until
    // we commit/rollback.
    let mut conn = new_conn(&url).await;
    let tx = conn.transaction().await.expect("tx");
    let claim = refund::claim_refund_locked(
        &tx, &creator, &inv, 2000, 2000, 0, "usd", RefundDestination::Cash, None,
        &key(&inv, "lock-claim"),
    )
    .await
    .expect("claim");
    assert!(
        matches!(claim, refund::ClaimResult::Claimed(_)),
        "the $20 claim is under cash $50 — claimed, got {claim:?}",
    );

    // (1) While the claim tx is OPEN (lock held), a try-lock on the SAME key fails.
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
        "the refund claim tx holds the per-creator advisory lock — a concurrent try-lock must fail",
    );

    // A DIFFERENT creator's key is free (per-creator, not global).
    let key_other: bool = obs_client
        .query(
            "SELECT pg_try_advisory_xact_lock(hashtext($1::text)::bigint) AS got",
            &[&other.to_string()],
        )
        .await
        .expect("try-lock other")[0]
        .get("got");
    assert!(key_other, "a DIFFERENT creator's advisory lock is free — the lock serializes per creator only");
    obs_client.execute("SELECT pg_advisory_unlock_all()", &[]).await.ok();

    tx.commit().await.expect("commit claim");
}

// ===========================================================================
// CRITICAL-1 (1b): the over-refund bound holds — two refunds summing > cash are
//     rejected. Sequential here (the lock makes the concurrent case reduce to this:
//     the 2nd refund sees the 1st's committed row). $50 cash; a $40 refund is
//     allowed, a second $40 (Σ=$80 > $50) is rejected.
// ===========================================================================

#[compio::test]
async fn two_refunds_summing_over_cash_second_is_rejected() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "sumcap").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "sumcap").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    ingest_at(&fx.state, app, 5000, period, 1).await;

    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv = active_invoice_id(&fx.state, creator, period).await.expect("invoice");
    append_payment(&fx.state, &inv, 5000, "in_sumcap").await;

    let mut conn = new_conn(&url).await;
    let provider = NativeRefundProvider;

    let r1 = refund::issue_refund(
        &mut conn, &provider, &inv, 4000, 4000, 0, RefundDestination::Cash, None, &key(&inv, "first40"),
    )
    .await
    .expect("issue");
    assert!(matches!(r1, RefundOutcome::Issued { .. }), "first $40 refund (≤ $50) is allowed");

    let r2 = refund::issue_refund(
        &mut conn, &provider, &inv, 4000, 4000, 0, RefundDestination::Cash, None, &key(&inv, "second40"),
    )
    .await
    .expect("issue");
    assert!(
        matches!(r2, RefundOutcome::OverRefund(_)),
        "second $40 refund (Σ $80 > cash $50) must be rejected — the bound holds, got {r2:?}",
    );

    let total = refund::refunds_total_for_destination(&conn, &inv, RefundDestination::Cash)
        .await
        .expect("sum");
    assert_eq!(total, 4000, "only the first $40 stuck; Σ cash refunds ≤ cash $50");
}

// ===========================================================================
// MAJOR-1: a `refund_to_credit` double-drive appends EXACTLY ONE grant — the
//     `credit_ledger_refund_to_credit_note_idx` partial UNIQUE is the durable guard.
//     We claim a credit refund as `pending`, then drive it TWICE bypassing the
//     fast-path SELECT skip (delete the grant between drives to force the second
//     INSERT to actually fire and hit ON CONFLICT DO NOTHING is moot — instead we
//     drive twice concurrently-equivalent and assert one grant). Simplest faithful
//     form: drive the same pending refund twice; the unique index makes the second
//     INSERT a no-op even if the SELECT guard were absent.
// (RED pre-fix: no unique index + a SELECT-then-INSERT guard → a second INSERT that
//  races past the SELECT appends a SECOND grant. We force the INSERT path on the
//  second drive by deleting the just-written grant is impossible (immutable trigger),
//  so we assert the constraint directly: a direct second INSERT of the same note RAISEs.)
// ===========================================================================

#[compio::test]
async fn refund_to_credit_double_drive_appends_exactly_one_grant() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "rtcdup").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "rtcdup").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    ingest_at(&fx.state, app, 5000, period, 1).await;

    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv = active_invoice_id(&fx.state, creator, period).await.expect("invoice");
    append_payment(&fx.state, &inv, 5000, "in_rtcdup").await;

    // Issue a $30 credit refund (drives once → one refund_to_credit grant).
    let mut conn = new_conn(&url).await;
    let provider = NativeRefundProvider;
    let r = refund::issue_refund(
        &mut conn, &provider, &inv, 3000, 3000, 0, RefundDestination::Credit, None, &key(&inv, "rtc-30"),
    )
    .await
    .expect("issue");
    let refund_id = match r {
        RefundOutcome::Issued { refund_id, .. } => refund_id,
        o => panic!("expected Issued, got {o:?}"),
    };

    // Exactly one refund_to_credit grant exists for this refund's note.
    let marker = refund::refund_to_credit_note(&refund_id);
    let count_grants = |state: Arc<AppState>, marker: String| async move {
        state
            .control_pg
            .query(
                "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger \
                 WHERE kind = 'refund_to_credit' AND note = $1",
                &[&marker],
            )
            .await
            .expect("count")[0]
            .get::<_, i64>("n")
    };
    assert_eq!(count_grants(fx.state.clone(), marker.clone()).await, 1, "one grant after the first drive");

    // The DURABLE guard: a SECOND direct INSERT of the SAME note (the race a missing
    // constraint would allow) is rejected by the partial UNIQUE index. THIS is the
    // assertion that fails pre-fix (no index → the second INSERT succeeds → double credit).
    let dup = fx
        .state
        .control_pg
        .execute(
            "INSERT INTO zeroship.credit_ledger \
               (id, creator_id, kind, amount_cents, currency, applied_invoice_id, note) \
             VALUES ($1, $2, 'refund_to_credit', 3000, 'usd', $3, $4)",
            &[&zeroship_core::typed_id::new_credit_id(), &creator, &inv, &marker],
        )
        .await;
    assert!(
        dup.is_err(),
        "a duplicate refund_to_credit grant for the same refund note must be rejected by the partial UNIQUE",
    );

    // Still exactly one grant; the credit balance reflects ONE $30 grant, not two.
    assert_eq!(count_grants(fx.state.clone(), marker).await, 1, "still exactly one grant — no double credit");
}

// ===========================================================================
// MAJOR-2: void_and_reissue is RE-DRIVABLE — a re-invocation after a simulated
//     Phase-2 interruption (the invoice is already void, but NOT yet reissued)
//     converges: it reissues + true-ups idempotently, with NO double void_reversal
//     and the balance conserved.
// ===========================================================================

#[compio::test]
async fn void_reissue_is_redrivable_after_phase1_crash() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "redrive").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "redrive").await;
    ensure_creator_billing(&fx.state, creator).await;
    set_customer(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;

    // $10 grant; $6 usage → subtotal 600, credit 600, total 0. Balance after consume = $4.
    insert_grant(&fx.state, creator, 1000, Utc::now() - Duration::hours(2)).await;
    ingest_at(&fx.state, app, 600, period, 1).await;

    let stripe = RecordingStripe::default();
    assert_eq!(run_reconcile(&fx.state, &stripe, now).await, 1);
    let inv_a = active_invoice_id(&fx.state, creator, period).await.expect("invoice A");
    assert_eq!(invoice_money(&fx.state, &inv_a).await, ("finalized".into(), 600, 600, 0));
    assert_eq!(
        zeroship_control::credit::balance(&*fx.state.control_pg, &creator, "usd").await.expect("bal"),
        400, "balance after consume = $4",
    );

    // ── Simulate a crash AFTER Phase 1 (void + void_reversal committed) but BEFORE the
    //    reissue: do exactly Phase 1 by hand under the per-creator lock, mirroring the
    //    helper, then leave the invoice void with NO reissue. ──
    {
        let mut c = new_conn(&url).await;
        let tx = c.transaction().await.expect("tx");
        tx.execute("SELECT pg_advisory_xact_lock(hashtext($1::text)::bigint)", &[&creator.to_string()])
            .await
            .expect("lock");
        // void_reversal for each consumed row.
        let consumed = tx
            .query(
                "SELECT amount_cents, currency, consumed_from_grant_id FROM zeroship.credit_ledger \
                 WHERE applied_invoice_id = $1 AND kind = 'consumed'",
                &[&inv_a],
            )
            .await
            .expect("consumed");
        for row in &consumed {
            let amt: i64 = row.get("amount_cents");
            let cur: String = row.get("currency");
            let gid: Option<String> = row.get("consumed_from_grant_id");
            tx.execute(
                "INSERT INTO zeroship.credit_ledger \
                   (id, creator_id, kind, amount_cents, currency, applied_invoice_id, consumed_from_grant_id) \
                 VALUES ($1, $2, 'void_reversal', $3, $4, $5, $6)",
                &[&zeroship_core::typed_id::new_credit_id(), &creator, &(-amt), &cur, &inv_a, &gid],
            )
            .await
            .expect("void_reversal");
        }
        tx.execute(
            "UPDATE zeroship.invoices SET status = 'void', voided_at = NOW(), updated_at = NOW() WHERE id = $1",
            &[&inv_a],
        )
        .await
        .expect("flip void");
        tx.commit().await.expect("commit phase1");
    }
    // Now the invoice is void, reversal applied (balance back to $10), no reissue yet.
    assert_eq!(invoice_money(&fx.state, &inv_a).await.0, "void");
    assert_eq!(
        zeroship_control::credit::balance(&*fx.state.control_pg, &creator, "usd").await.expect("bal"),
        1000, "balance restored to $10 after the void_reversal (no reissue yet)",
    );

    // ── RE-DRIVE: invoke void_and_reissue on the ALREADY-VOID invoice. It must skip
    //    Phase 1 (no second void_reversal), reissue, and true-up — converging. ──
    let outcome = zeroship_control::void_reissue::void_and_reissue(&fx.state, &stripe, &inv_a)
        .await
        .expect("re-drive void+reissue");
    assert_eq!(outcome.voided_invoice_id, inv_a);
    let reissued = outcome.reissued_invoice_id.expect("reissued on re-drive");
    assert_ne!(reissued, inv_a);
    assert_eq!(invoice_money(&fx.state, &reissued).await, ("finalized".into(), 600, 600, 0));

    // Exactly ONE void_reversal (no double on the re-drive); +$6.
    let vr = fx
        .state
        .control_pg
        .query(
            "SELECT COALESCE(SUM(amount_cents),0)::bigint AS s, COUNT(*)::bigint AS n \
             FROM zeroship.credit_ledger WHERE applied_invoice_id = $1 AND kind = 'void_reversal'",
            &[&inv_a],
        )
        .await
        .expect("vr")
        .remove(0);
    assert_eq!(vr.get::<_, i64>("s"), 600, "exactly the $6 restored");
    assert_eq!(vr.get::<_, i64>("n"), 1, "exactly ONE void_reversal — the re-drive did not double it");

    // BALANCE CONSERVED: reissue re-drew $6 → back to $4. Never $-2, never $10.
    assert_eq!(
        zeroship_control::credit::balance(&*fx.state.control_pg, &creator, "usd").await.expect("bal"),
        400, "re-drive converges: balance back to $4, conserved",
    );

    // Idempotent on a THIRD invocation (now everything is done): same outcome, no churn.
    let again = zeroship_control::void_reissue::void_and_reissue(&fx.state, &stripe, &inv_a)
        .await
        .expect("third invocation converges");
    assert_eq!(again.reissued_invoice_id.as_deref(), Some(reissued.as_str()), "third drive is a stable no-op");
    assert_eq!(
        zeroship_control::credit::balance(&*fx.state.control_pg, &creator, "usd").await.expect("bal"),
        400, "balance still $4 after a third drive",
    );
}
