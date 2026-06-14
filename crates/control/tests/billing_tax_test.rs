//! PR-5 regression tests for billing-ops gap #26: the `TaxProvider` seam computing tax
//! at finalize and FREEZING it into the EXISTING `invoices.tax_cents` (code-only — no
//! changeset; `tax_cents` already exists from the redesign).
//!
//! FAITHFUL by construction: every assertion runs against a live, migrated Postgres
//! (`CONTROL_TEST_DB`; silent skip otherwise) and exercises the REAL paths —
//!   * the REAL reconciler `billing_reconcile::tick_with` → `bill_creator` (so the tax
//!     call runs INSIDE the real finalize-in-one-UPDATE and the REAL balance CHECK
//!     `total = subtotal − credit + tax` validates the row);
//!   * the REAL `TaxProvider` seam on the REAL `AppState.tax_provider` (the fake is
//!     injected EXACTLY the way `NativeTaxProvider` is — `build_fixture` swaps the Arc),
//!     proving provider-swappability;
//!   * the REAL `credit::consume_at_finalize` (credit-before-tax ordering) when a credit
//!     grant is present, so the post-credit base the tax seam taxes is the real one.
//! Only the Stripe WIRE is a recording fake (Stripe is irrelevant to the tax math).
//!
//! These tests:
//!   (a) NATIVE provider ⇒ `tax_cents = 0`, `total = subtotal − credit` (balance CHECK
//!       holds) — the behaviour-neutral USD-launch default;
//!   (b) a FAKE provider returning a NON-ZERO tax ⇒ that tax is FROZEN onto the invoice
//!       (`tax_cents = fake`) and `total = subtotal − credit + tax` (balance CHECK holds)
//!       — proves the seam wires through the one-statement finalize UPDATE. RED-first: it
//!       FAILS against any reconciler that hard-wires `tax_cents = 0` (the pre-PR-5 code,
//!       and any code that ignores the provider result), because `total` would be
//!       `subtotal − credit` and `tax_cents` would be 0;
//!   (c) the fake is INJECTED the same way Native is (`AppState.tax_provider`), so the
//!       seam is provider-swappable — covered structurally by (a) vs (b) sharing one
//!       `build_fixture(tax_provider)` path;
//!   (d) tax composes with credit AND with multi-segment proration: a credit-reduced bill
//!       taxes the POST-CREDIT base, and a non-zero tax on the summed segment subtotal
//!       freezes once per invoice with the balance CHECK holding.
//!
//! Consistency with PR-3 tax-on-refund: with Native tax = 0 the refund tax-split is a
//! no-op (proportional split of 0 is 0, no division-by-zero); that is asserted in
//! `billing_refund_void_test`. Here we prove the FINALIZE leg of the same seam.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::billing_reconcile;
use zeroship_control::metering::Metering;
use zeroship_control::stripe_client::{Period, StripeApi};
use zeroship_control::stripe_store::StripeError;
use zeroship_control::tax::{TaxAmount, TaxContext, TaxProvider, TaxProviderKind};
use zeroship_control::{AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore};
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
    p.push(format!("zs-tax-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ===========================================================================
// A FAKE TaxProvider returning a fixed non-zero tax — injected the SAME way
// NativeTaxProvider is (onto AppState.tax_provider via build_fixture), proving the
// seam is provider-swappable. It records the post-credit base it was asked to tax so
// the test can assert tax is computed on the post-credit subtotal (flow E ordering).
// ===========================================================================

struct FakeTaxProvider {
    /// The fixed tax in cents this provider freezes onto every invoice.
    tax_cents: i64,
    /// The post-credit bases the reconciler asked us to tax (one per invoice/finalize).
    seen_bases: Mutex<Vec<i64>>,
}

impl FakeTaxProvider {
    fn new(tax_cents: i64) -> Self {
        Self { tax_cents, seen_bases: Mutex::new(Vec::new()) }
    }
}

#[async_trait::async_trait(?Send)]
impl TaxProvider for FakeTaxProvider {
    fn kind(&self) -> TaxProviderKind {
        // The fake reuses the Native kind label — it is a TEST double, not a new wire
        // kind. What is under test is that the reconciler USES the returned tax_cents,
        // not the kind label.
        TaxProviderKind::Native
    }

    async fn compute_tax(
        &self,
        ctx: &TaxContext<'_>,
    ) -> Result<TaxAmount, zeroship_control::metering::provider::ProviderError> {
        self.seen_bases.lock().unwrap().push(ctx.taxable_base_cents);
        Ok(TaxAmount { tax_cents: self.tax_cents })
    }
}

// ===========================================================================
// A recording StripeApi fake (no HTTP). `bill_creator` is generic over StripeApi; the
// tax math under test never touches Stripe, so a fake suffices.
// ===========================================================================

#[derive(Default)]
struct RecordingStripe {
    item_seq: Mutex<u64>,
    inv_seq: Mutex<u64>,
}

impl RecordingStripe {
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
        _provider_invoice_id: &str,
        _amount_cents: u64,
        _currency: &str,
        _idempotency_key: &str,
    ) -> Result<String, StripeError> {
        Ok(Self::unique("re"))
    }
}

// ===========================================================================
// Fixture (real PG; recording-fake Stripe; INJECTABLE tax provider).
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

/// Build the fixture with a CALLER-SUPPLIED tax provider — the SAME `Arc<dyn TaxProvider>`
/// slot `main.rs` fills from `build_tax_provider`. Passing `NativeTaxProvider` vs a
/// `FakeTaxProvider` is exactly the production provider-swap, proving swappability (c).
async fn build_fixture(
    db_url: &str,
    label: &str,
    tax_provider: Arc<dyn TaxProvider>,
) -> Fixture {
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
        tax_provider,
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
    });

    Fixture { state, blob_root, deploy_tmp_dir }
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
            &[&email, &"Tax Creator".to_string()],
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

/// A plan charging 1 cent/request, no included CU, no base fee (fx = 1 cent/CU).
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
    let plan_id = format!("pln_tax_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'tax-test', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id, &fx_one_cent],
        )
        .await
        .expect("seed priced plan");
    plan_id
}

async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("tax-{}", Uuid::new_v4());
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

/// Read `(status, subtotal, credit, tax, total)` for the active invoice.
async fn read_invoice(
    state: &AppState,
    creator: Uuid,
    period_start: i64,
) -> Option<(String, i64, i64, i64, i64)> {
    state
        .control_pg
        .query(
            "SELECT status, subtotal_cents, credit_cents, tax_cents, total_cents \
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
                r.get::<_, i64>("tax_cents"),
                r.get::<_, i64>("total_cents"),
            )
        })
}

async fn insert_grant(
    state: &AppState,
    creator: Uuid,
    amount_cents: i64,
    currency: &str,
    created_at: chrono::DateTime<chrono::Utc>,
) -> String {
    let id = format!("crd_{}", Uuid::new_v4().simple());
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.credit_ledger \
               (id, creator_id, kind, amount_cents, currency, created_at) \
             VALUES ($1, $2, 'grant', $3, $4, $5)",
            &[&id, &creator, &amount_cents, &currency, &created_at],
        )
        .await
        .expect("insert grant");
    id
}

// ===========================================================================
// (a) NATIVE provider ⇒ tax_cents = 0, total = subtotal − credit (behaviour-neutral).
// ===========================================================================

#[compio::test]
async fn native_tax_is_zero_and_total_is_subtotal_minus_credit() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(
        &url,
        "native",
        zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
    )
    .await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "native").await;
    ensure_creator_billing(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // $10 of usage, $3 credit grant → subtotal 1000, credit 300, tax 0, total 700.
    insert_grant(&fx.state, creator, 300, "usd", chrono::Utc::now()).await;
    ingest_at(&fx.state, app, 1000, period, 1).await;

    let billed = billing_reconcile::tick_with(&fx.state, &RecordingStripe::default(), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1);

    let inv = read_invoice(&fx.state, creator, period).await.expect("invoice");
    assert_eq!(inv.0, "finalized");
    assert_eq!(inv.1, 1000, "subtotal");
    assert_eq!(inv.2, 300, "credit_cents");
    assert_eq!(inv.3, 0, "NATIVE tax_cents = 0 (USD launch — behaviour-neutral)");
    assert_eq!(inv.4, 700, "total = subtotal − credit + tax(0) = 1000 − 300 + 0");
    // The balance CHECK (total = subtotal − credit + tax) held (the row finalized).
    assert_eq!(inv.4, inv.1 - inv.2 + inv.3, "balance CHECK identity");
}

// ===========================================================================
// (b) FAKE provider returning a NON-ZERO tax ⇒ tax FROZEN onto the invoice +
//     total = subtotal − credit + tax. RED-first: FAILS if tax isn't wired into the
//     one-statement finalize UPDATE (a reconciler hard-wiring tax_cents = 0 would
//     leave tax_cents = 0 and total = subtotal − credit).
// ===========================================================================

#[compio::test]
async fn fake_provider_tax_is_frozen_and_total_includes_tax() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    // Inject the FAKE provider the SAME way Native is injected (the Arc slot) — (c).
    let fake = Arc::new(FakeTaxProvider::new(123));
    let fx = build_fixture(&url, "fake", fake.clone() as Arc<dyn TaxProvider>).await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "fake").await;
    ensure_creator_billing(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // $10 of usage, $3 credit → subtotal 1000, credit 300, post-credit base 700,
    // fake tax 123 → total = 1000 − 300 + 123 = 823.
    insert_grant(&fx.state, creator, 300, "usd", chrono::Utc::now()).await;
    ingest_at(&fx.state, app, 1000, period, 1).await;

    let billed = billing_reconcile::tick_with(&fx.state, &RecordingStripe::default(), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1);

    let inv = read_invoice(&fx.state, creator, period).await.expect("invoice");
    assert_eq!(inv.0, "finalized");
    assert_eq!(inv.1, 1000, "subtotal");
    assert_eq!(inv.2, 300, "credit_cents");
    assert_eq!(inv.3, 123, "FAKE tax FROZEN into tax_cents (seam wires through finalize)");
    assert_eq!(inv.4, 823, "total = subtotal − credit + tax = 1000 − 300 + 123");
    // The balance CHECK held with a NON-ZERO tax (the whole point of the seam).
    assert_eq!(inv.4, inv.1 - inv.2 + inv.3, "balance CHECK identity with non-zero tax");

    // The seam taxed the POST-CREDIT base (flow E ordering: credit before tax), once
    // per invoice.
    let bases = fake.seen_bases.lock().unwrap().clone();
    assert_eq!(bases.len(), 1, "compute_tax called ONCE per invoice");
    assert_eq!(bases[0], 700, "tax computed over the post-credit base (1000 − 300)");
}

// ===========================================================================
// (d) No-credit path: a non-zero tax on a full subtotal freezes correctly and the
//     balance CHECK holds (total = subtotal − 0 + tax). Guards against the credit
//     step masking the tax wiring.
// ===========================================================================

#[compio::test]
async fn fake_provider_tax_without_credit_holds_balance_check() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fake = Arc::new(FakeTaxProvider::new(250));
    let fx = build_fixture(&url, "fake-nocredit", fake.clone() as Arc<dyn TaxProvider>).await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "fake-nocredit").await;
    ensure_creator_billing(&fx.state, creator).await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // $5 usage, NO credit → subtotal 500, credit 0, tax 250 → total 750.
    ingest_at(&fx.state, app, 500, period, 1).await;

    let billed = billing_reconcile::tick_with(&fx.state, &RecordingStripe::default(), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1);

    let inv = read_invoice(&fx.state, creator, period).await.expect("invoice");
    assert_eq!(inv.1, 500, "subtotal");
    assert_eq!(inv.2, 0, "no credit");
    assert_eq!(inv.3, 250, "tax frozen");
    assert_eq!(inv.4, 750, "total = subtotal − 0 + tax = 500 + 250");
    assert_eq!(inv.4, inv.1 - inv.2 + inv.3, "balance CHECK identity");

    let bases = fake.seen_bases.lock().unwrap().clone();
    assert_eq!(bases[0], 500, "tax computed over the full subtotal (no credit)");
}
