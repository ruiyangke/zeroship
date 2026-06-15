//! Integration tests for the Stripe state-reconciliation cron (#28 "Stripe reconciliation")
//! — the production backstop that catches drift when Stripe webhooks are
//! missed/dropped/out-of-order.
//!
//! FAITHFUL by construction: the tests drive the REAL `cyper`-based `StripeClient` against a
//! localhost **mock-Stripe HTTP server** that speaks Stripe's GET/JSON protocol and models
//! the DRIFT cases the reconciler must catch. There is NO stubbed client object on the wire
//! path — the reconciler builds a real `StripeClient` pointed at the mock via
//! `AppState.stripe_base_url`, so the request building, the `Authorization: Bearer` +
//! `Stripe-Version` headers, the HTTP round-trip and the JSON parse are all exercised end to
//! end. The reconcile core (`stripe_reconcile::tick_with`) runs against a live, migrated
//! Postgres (`CONTROL_TEST_DB`; silent skip otherwise) with real `invoices` / `refunds` /
//! `billing_disputes` / `billing_provider_refs` / `billing_reconciliation_findings` rows.
//!
//! The required cases (each seeds its OWN creator + globally-unique Stripe ids, so the
//! DEFAULT parallel cargo runner never causes cross-test interference):
//!   (a) Stripe says an invoice is PAID but we have no charge row → `missed_invoice_payment`.
//!   (b) Stripe says a refund `failed` but we hold it `issued` → `refund_status_drift`.
//!   (c) a Stripe dispute we have no internal row for → `missing_dispute` (+ the gated
//!       backstop parks/resolves it when enabled).
//!   (d) a fully-consistent invoice/refund/dispute → NO finding (no false positives).
//!   (e) idempotency — a second sweep does NOT duplicate findings.
//!   (f) the advisory lock single-flights — a concurrent tick (lock already held) no-ops.

#![allow(clippy::future_not_send)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::stripe_reconcile::{self, ReconcileConfig};
use zeroship_control::{
    token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// `stripe_reconcile::tick_with` single-flights fleet-wide via `pg_try_advisory_lock` (the
/// production multi-instance safety). Under the DEFAULT multi-threaded cargo runner two
/// sweep-driving tests would race the SAME lock: the loser gets `ran: false` and does no
/// detection, so its finding assertions would fail. Serialize the sweep-driving tests with
/// a process-wide lock to mirror the production single-flight — exactly as
/// `billing_reconcile_test`'s `RECONCILE_LOCK` does (a poisoned lock from a prior panic is
/// recovered). The single-flight test (f), which deliberately holds the advisory lock on a
/// side session, is serialized too so it never starves a concurrent sweep test.
static RECONCILE_LOCK: Mutex<()> = Mutex::new(());

fn serialize_sweeps() -> std::sync::MutexGuard<'static, ()> {
    RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-recon-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

fn short() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

// ===========================================================================
// Mock-Stripe HTTP server — a real localhost server the REAL cyper client hits.
// Models the four reconciliation GETs (invoice / refund / dispute retrieve + dispute
// list) and lets a test program DRIFT into each via `set_*`.
// ===========================================================================

#[derive(Clone, Default)]
struct MockInvoice {
    status: String,
    amount_due: i64,
    amount_paid: i64,
    total: i64,
}

#[derive(Clone, Default)]
struct MockRefund {
    status: String,
    amount: i64,
}

#[derive(Clone, Default)]
struct MockDispute {
    status: String,
    amount: i64,
    currency: String,
    charge: Option<String>,
    payment_intent: Option<String>,
    reason: Option<String>,
}

#[derive(Default)]
struct MockState {
    /// GET path → recorded count (proves the wire path was hit).
    get_counts: HashMap<String, usize>,
    invoices: HashMap<String, MockInvoice>,    // in_… → shape
    refunds: HashMap<String, MockRefund>,      // re_… → shape
    disputes: HashMap<String, MockDispute>,    // du_… → shape (retrieve)
    /// Disputes returned by `GET /v1/disputes?created>=…` (the list/window scan).
    listed_disputes: Vec<(String, MockDispute)>,
    /// True iff a Stripe-Version header was missing on ANY request (a faithful 400).
    saw_unpinned: bool,
}

#[derive(Clone)]
struct MockStripe {
    state: Arc<Mutex<MockState>>,
    base_url: String,
}

impl MockStripe {
    fn set_invoice(&self, id: &str, inv: MockInvoice) {
        self.state.lock().unwrap().invoices.insert(id.to_string(), inv);
    }
    fn set_refund(&self, id: &str, re: MockRefund) {
        self.state.lock().unwrap().refunds.insert(id.to_string(), re);
    }
    fn set_dispute(&self, id: &str, d: MockDispute) {
        self.state.lock().unwrap().disputes.insert(id.to_string(), d);
    }
    fn list_dispute(&self, id: &str, d: MockDispute) {
        let mut st = self.state.lock().unwrap();
        st.disputes.insert(id.to_string(), d.clone());
        st.listed_disputes.push((id.to_string(), d));
    }
    fn get_count(&self, path_prefix: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.get_counts
            .iter()
            .filter(|(p, _)| p.starts_with(path_prefix))
            .map(|(_, c)| *c)
            .sum()
    }
    fn saw_unpinned(&self) -> bool {
        self.state.lock().unwrap().saw_unpinned
    }
}

async fn start_mock_stripe() -> MockStripe {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");
    let state = Arc::new(Mutex::new(MockState::default()));
    let accept_state = Arc::clone(&state);
    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else { break };
            let conn_state = Arc::clone(&accept_state);
            compio::runtime::spawn(async move {
                serve_conn(stream, conn_state).await;
            })
            .detach();
        }
    })
    .detach();
    MockStripe { state, base_url }
}

async fn serve_conn(mut stream: TcpStream, state: Arc<Mutex<MockState>>) {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        loop {
            let Some((method, path, version_pinned, consumed)) = try_parse(&acc) else { break };
            acc.drain(0..consumed);
            let response = handle(&method, &path, version_pinned, &state);
            if stream.write_all(response).await.0.is_err() {
                return;
            }
        }
        let buf = vec![0u8; 4096];
        let compio::BufResult(n, buf) = stream.read(buf).await;
        match n {
            Ok(0) | Err(_) => return,
            Ok(read) => acc.extend_from_slice(&buf[..read]),
        }
    }
}

/// Parse ONE complete HTTP request: returns `(method, path, stripe_version_present, consumed)`.
fn try_parse(buf: &[u8]) -> Option<(String, String, bool, usize)> {
    let text = std::str::from_utf8(buf).ok()?;
    let header_end = text.find("\r\n\r\n")?;
    let head = &text[..header_end];
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut content_length = 0usize;
    let mut version_pinned = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            match key.as_str() {
                "content-length" => content_length = v.trim().parse().unwrap_or(0),
                "stripe-version" => version_pinned = !v.trim().is_empty(),
                _ => {}
            }
        }
    }
    let body_start = header_end + 4;
    if buf.len() < body_start + content_length {
        return None;
    }
    Some((method, path, version_pinned, body_start + content_length))
}

fn handle(method: &str, path: &str, version_pinned: bool, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
    // C1: a faithful Stripe requires the pinned Stripe-Version header on EVERY call.
    if !version_pinned {
        state.lock().unwrap().saw_unpinned = true;
        return http_json(400, r#"{"error":{"code":"version_unpinned","message":"missing Stripe-Version"}}"#);
    }
    if method != "GET" {
        return http_json(405, r#"{"error":{"code":"method_not_allowed"}}"#);
    }
    {
        let mut st = state.lock().unwrap();
        *st.get_counts.entry(path.to_string()).or_insert(0) += 1;
    }
    let id_segment = |prefix: &str| -> String {
        path.trim_start_matches(prefix).split('?').next().unwrap_or("").to_string()
    };

    // GET /v1/disputes?created[gte]=…  — the window scan (must be checked BEFORE the
    // single-dispute retrieve since both start with /v1/disputes).
    if path.starts_with("/v1/disputes?") || path == "/v1/disputes" {
        let st = state.lock().unwrap();
        let data: Vec<String> = st.listed_disputes.iter().map(|(id, d)| dispute_json(id, d)).collect();
        return http_json(200, &format!(r#"{{"object":"list","data":[{}]}}"#, data.join(",")));
    }
    if path.starts_with("/v1/disputes/") {
        let id = id_segment("/v1/disputes/");
        let st = state.lock().unwrap();
        let Some(d) = st.disputes.get(&id) else {
            return http_json(404, r#"{"error":{"code":"resource_missing"}}"#);
        };
        return http_json(200, &dispute_json(&id, d));
    }
    if path.starts_with("/v1/invoices/") {
        let id = id_segment("/v1/invoices/");
        let st = state.lock().unwrap();
        let Some(inv) = st.invoices.get(&id) else {
            return http_json(404, r#"{"error":{"code":"resource_missing"}}"#);
        };
        return http_json(
            200,
            &format!(
                r#"{{"id":"{id}","object":"invoice","status":"{}","amount_due":{},"amount_paid":{},"total":{}}}"#,
                inv.status, inv.amount_due, inv.amount_paid, inv.total
            ),
        );
    }
    if path.starts_with("/v1/refunds/") {
        let id = id_segment("/v1/refunds/");
        let st = state.lock().unwrap();
        let Some(re) = st.refunds.get(&id) else {
            return http_json(404, r#"{"error":{"code":"resource_missing"}}"#);
        };
        return http_json(
            200,
            &format!(
                r#"{{"id":"{id}","object":"refund","status":"{}","amount":{}}}"#,
                re.status, re.amount
            ),
        );
    }
    http_json(404, r#"{"error":{"code":"resource_missing"}}"#)
}

fn dispute_json(id: &str, d: &MockDispute) -> String {
    let opt = |o: &Option<String>| o.as_deref().map_or("null".to_string(), |s| format!("\"{s}\""));
    format!(
        r#"{{"id":"{id}","object":"dispute","status":"{}","amount":{},"currency":"{}","charge":{},"payment_intent":{},"reason":{}}}"#,
        d.status, d.amount, if d.currency.is_empty() { "usd" } else { &d.currency },
        opt(&d.charge), opt(&d.payment_intent), opt(&d.reason),
    )
}

fn http_json(status: u16, json: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let body = json.as_bytes();
    let mut resp = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body);
    resp
}

// ===========================================================================
// Fixture: real PG + the mock-Stripe base URL wired into AppState.
// ===========================================================================

struct Fixture {
    state: Arc<AppState>,
    mock: MockStripe,
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
    let mock = start_mock_stripe().await;
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls).await.expect("control-pg");
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
        stripe_base_url: mock.base_url.clone(),
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
        static_policies: zeroship_authz::load_platform_policies().expect("authz policies"),
        pat_issuer: Arc::new(token_handlers::PatIssuer::dev_insecure()),
        hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new("http://127.0.0.1:9")),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: Arc::new(zeroship_control::billing_read::ProjectedChargeCache::default()),
    });

    Fixture { state, mock, blob_root, deploy_tmp_dir }
}

// ---------------------------------------------------------------------------
// Seeding helpers.
// ---------------------------------------------------------------------------

async fn make_creator(state: &AppState, label: &str) -> Uuid {
    let email = format!("{label}-{}@example.test", Uuid::new_v4().simple());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, 'Recon Creator') RETURNING id",
            &[&email],
        )
        .await
        .expect("insert user");
    let creator: Uuid = rows[0].get("id");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
            &[&creator],
        )
        .await
        .expect("insert creator_billing");
    creator
}

/// Seed a finalized invoice + its Stripe `in_…` provider ref. Returns `(our_invoice_id, in_…)`.
async fn seed_finalized_invoice(state: &AppState, creator: Uuid, total: i64) -> (String, String) {
    let inv = zeroship_core::typed_id::new_invoice_id();
    let stripe_in = format!("in_recon_{}", short());
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices \
               (id, creator_id, period, status, subtotal_cents, credit_cents, tax_cents, \
                total_cents, finalized_at) \
             VALUES ($1, $2, DATE '2026-05-01', 'finalized', $3, 0, 0, $3, NOW())",
            &[&inv, &creator, &total],
        )
        .await
        .expect("seed invoice");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
             VALUES ($1, 'stripe', 'invoice', $2)",
            &[&inv, &stripe_in],
        )
        .await
        .expect("seed invoice ref");
    (inv, stripe_in)
}

/// Append a `charge` invoice_payment row (the cash `invoice.paid` records).
async fn append_charge(state: &AppState, invoice_id: &str, amount: i64, provider_ref: &str) {
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoice_payments (id, invoice_id, amount_cents, currency, kind, provider_ref) \
             VALUES ($1, $2, $3, 'usd', 'charge', $4)",
            &[&zeroship_core::typed_id::new_invoice_payment_id(), &invoice_id, &amount, &provider_ref],
        )
        .await
        .expect("append charge");
}

/// Seed an `issued` refund + its Stripe `re_…` provider ref. Returns `(our_refund_id, re_…)`.
async fn seed_issued_refund(state: &AppState, invoice_id: &str, amount: i64) -> (String, String) {
    let rf = zeroship_core::typed_id::new_refund_id();
    let stripe_re = format!("re_recon_{}", short());
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.refunds \
               (id, invoice_id, amount_cents, subtotal_cents, tax_cents, currency, destination, \
                idempotency_key, request_fingerprint, status, issued_at) \
             VALUES ($1, $2, $3, $3, 0, 'usd', 'cash', $4, 'fp', 'issued', NOW())",
            &[&rf, &invoice_id, &amount, &format!("idem-{}", short())],
        )
        .await
        .expect("seed refund");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.refund_provider_refs (refund_id, provider, ref_kind, external_id) \
             VALUES ($1, 'stripe', 'refund', $2)",
            &[&rf, &stripe_re],
        )
        .await
        .expect("seed refund ref");
    (rf, stripe_re)
}

/// Count findings of a given kind for a given entity id (scopes the assertion to THIS test).
async fn finding_count(state: &AppState, kind: &str, entity_id: &str) -> i64 {
    state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_reconciliation_findings \
             WHERE kind = $1 AND entity_id = $2",
            &[&kind, &entity_id],
        )
        .await
        .expect("count findings")[0]
        .get::<_, i64>("n")
}

fn cfg() -> ReconcileConfig {
    ReconcileConfig { window_days: 365, entity_cap: 1000, auto_heal_disputes: false }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ===========================================================================
// (a) missed invoice payment.
// ===========================================================================

#[compio::test]
async fn missed_invoice_payment_is_flagged() {
    let Some(url) = db_url() else { eprintln!("skip: CONTROL_TEST_DB not set"); return };
    let _sweep_guard = serialize_sweeps();
    let fx = build_fixture(&url, "missed-inv").await;
    let creator = make_creator(&fx.state, "missed-inv").await;
    let (_inv, stripe_in) = seed_finalized_invoice(&fx.state, creator, 5000).await;
    // Stripe says PAID with amount_paid>0; we recorded NO charge row → drift.
    fx.mock.set_invoice(&stripe_in, MockInvoice {
        status: "paid".into(), amount_due: 0, amount_paid: 5000, total: 5000,
    });

    let stripe = real_client(&fx);
    let summary = stripe_reconcile::tick_with(&fx.state, &stripe, now(), cfg()).await.expect("tick");

    assert!(summary.ran, "the sweep ran (held the advisory lock)");
    assert!(fx.mock.get_count("/v1/invoices/") >= 1, "the REAL client hit Stripe's invoice GET");
    assert!(!fx.mock.saw_unpinned(), "every call pinned Stripe-Version (C1)");
    assert_eq!(
        finding_count(&fx.state, "missed_invoice_payment", &stripe_in).await, 1,
        "Stripe-paid invoice with no charge row → exactly one missed_invoice_payment finding"
    );
}

// ===========================================================================
// (b) refund status drift.
// ===========================================================================

#[compio::test]
async fn refund_failed_at_stripe_but_issued_locally_is_flagged() {
    let Some(url) = db_url() else { eprintln!("skip: CONTROL_TEST_DB not set"); return };
    let _sweep_guard = serialize_sweeps();
    let fx = build_fixture(&url, "refund-drift").await;
    let creator = make_creator(&fx.state, "refund-drift").await;
    let (inv, _in) = seed_finalized_invoice(&fx.state, creator, 10_000).await;
    append_charge(&fx.state, &inv, 10_000, &format!("ch_{}", short())).await;
    let (_rf, stripe_re) = seed_issued_refund(&fx.state, &inv, 4000).await;
    // Stripe says the refund FAILED; we still hold it `issued` → drift.
    fx.mock.set_refund(&stripe_re, MockRefund { status: "failed".into(), amount: 4000 });

    let stripe = real_client(&fx);
    stripe_reconcile::tick_with(&fx.state, &stripe, now(), cfg()).await.expect("tick");

    assert!(fx.mock.get_count("/v1/refunds/") >= 1, "the REAL client hit Stripe's refund GET");
    assert_eq!(
        finding_count(&fx.state, "refund_status_drift", &stripe_re).await, 1,
        "Stripe-failed refund we hold issued → exactly one refund_status_drift finding"
    );
}

// ===========================================================================
// (c) missing dispute (+ gated backstop park).
// ===========================================================================

#[compio::test]
async fn stripe_dispute_with_no_internal_row_is_flagged() {
    let Some(url) = db_url() else { eprintln!("skip: CONTROL_TEST_DB not set"); return };
    let _sweep_guard = serialize_sweeps();
    let fx = build_fixture(&url, "missing-dispute").await;
    let _creator = make_creator(&fx.state, "missing-dispute").await;
    let du = format!("du_recon_{}", short());
    // A Stripe dispute in the window with NO billing_disputes / pending_disputes row.
    fx.mock.list_dispute(&du, MockDispute {
        status: "needs_response".into(), amount: 2500, currency: "usd".into(),
        charge: Some(format!("ch_{}", short())), payment_intent: Some(format!("pi_{}", short())),
        reason: Some("fraudulent".into()),
    });

    let stripe = real_client(&fx);
    stripe_reconcile::tick_with(&fx.state, &stripe, now(), cfg()).await.expect("tick");

    assert!(fx.mock.get_count("/v1/disputes") >= 1, "the REAL client hit Stripe's dispute list");
    assert_eq!(
        finding_count(&fx.state, "missing_dispute", &du).await, 1,
        "a Stripe dispute with no internal row → exactly one missing_dispute finding"
    );
    // FLAG-only by default: nothing parked.
    let parked: i64 = fx.state.control_pg
        .query("SELECT COUNT(*)::bigint AS n FROM zeroship.pending_disputes WHERE provider_dispute_id=$1", &[&du])
        .await.expect("count parked")[0].get("n");
    assert_eq!(parked, 0, "default is FLAG-only — no auto-heal park");
}

#[compio::test]
async fn missing_dispute_backstop_parks_when_enabled_and_linkage_exists() {
    let Some(url) = db_url() else { eprintln!("skip: CONTROL_TEST_DB not set"); return };
    let _sweep_guard = serialize_sweeps();
    let fx = build_fixture(&url, "dispute-heal").await;
    let creator = make_creator(&fx.state, "dispute-heal").await;
    let (inv, _in) = seed_finalized_invoice(&fx.state, creator, 8000).await;
    // The settling pi_ links to OUR invoice (what invoice.paid records) → the backstop can resolve.
    let pi = format!("pi_heal_{}", short());
    fx.state.control_pg
        .execute(
            "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
             VALUES ($1, 'stripe', 'payment_intent', $2)",
            &[&inv, &pi],
        )
        .await.expect("seed pi linkage");
    let du = format!("du_heal_{}", short());
    fx.mock.list_dispute(&du, MockDispute {
        status: "needs_response".into(), amount: 3000, currency: "usd".into(),
        charge: None, payment_intent: Some(pi.clone()), reason: None,
    });

    let stripe = real_client(&fx);
    let heal_cfg = ReconcileConfig { auto_heal_disputes: true, ..cfg() };
    let summary = stripe_reconcile::tick_with(&fx.state, &stripe, now(), heal_cfg).await.expect("tick");

    assert_eq!(finding_count(&fx.state, "missing_dispute", &du).await, 1, "still flagged");
    assert_eq!(summary.disputes_parked, 1, "the gated backstop parked the missed dispute");
    let parked: i64 = fx.state.control_pg
        .query("SELECT COUNT(*)::bigint AS n FROM zeroship.pending_disputes WHERE provider_dispute_id=$1", &[&du])
        .await.expect("count parked")[0].get("n");
    assert_eq!(parked, 1, "auto-heal parked the dispute (idempotent order-independent path)");
}

// ===========================================================================
// (d) fully-consistent — NO false positives.
// ===========================================================================

#[compio::test]
async fn fully_consistent_state_produces_no_findings() {
    let Some(url) = db_url() else { eprintln!("skip: CONTROL_TEST_DB not set"); return };
    let _sweep_guard = serialize_sweeps();
    let fx = build_fixture(&url, "consistent").await;
    let creator = make_creator(&fx.state, "consistent").await;

    // Consistent invoice: Stripe paid AND we recorded the charge.
    let (inv, stripe_in) = seed_finalized_invoice(&fx.state, creator, 6000).await;
    append_charge(&fx.state, &inv, 6000, &format!("ch_{}", short())).await;
    fx.mock.set_invoice(&stripe_in, MockInvoice {
        status: "paid".into(), amount_due: 0, amount_paid: 6000, total: 6000,
    });
    // Consistent refund: Stripe `succeeded`, we hold `issued`.
    let (_rf, stripe_re) = seed_issued_refund(&fx.state, &inv, 1000).await;
    fx.mock.set_refund(&stripe_re, MockRefund { status: "succeeded".into(), amount: 1000 });
    // Consistent dispute: Stripe `won` and we recorded `won` (status seeded directly).
    let du = format!("du_ok_{}", short());
    let dsp = zeroship_core::typed_id::new_dispute_id();
    fx.state.control_pg
        .execute(
            "INSERT INTO zeroship.billing_disputes (id, invoice_id, amount_cents, currency, status, provider_dispute_id, resolved_at) \
             VALUES ($1, $2, 2000, 'usd', 'won', $3, NOW())",
            &[&dsp, &inv, &du],
        )
        .await.expect("seed won dispute");
    fx.mock.set_dispute(&du, MockDispute {
        status: "won".into(), amount: 2000, currency: "usd".into(), charge: None, payment_intent: None, reason: None,
    });
    // It must NOT appear in the missing-dispute list scan (it is known).

    let stripe = real_client(&fx);
    let summary = stripe_reconcile::tick_with(&fx.state, &stripe, now(), cfg()).await.expect("tick");

    assert_eq!(finding_count(&fx.state, "missed_invoice_payment", &stripe_in).await, 0, "no invoice finding");
    assert_eq!(finding_count(&fx.state, "refund_status_drift", &stripe_re).await, 0, "no refund finding");
    assert_eq!(finding_count(&fx.state, "dispute_status_drift", &du).await, 0, "no dispute finding");
    assert_eq!(finding_count(&fx.state, "missing_dispute", &du).await, 0, "known dispute not flagged missing");
    // Scoped: this creator's entities contributed zero findings.
    let _ = summary;
}

// ===========================================================================
// (e) idempotency — a second sweep does not duplicate.
// ===========================================================================

#[compio::test]
async fn second_sweep_does_not_duplicate_findings() {
    let Some(url) = db_url() else { eprintln!("skip: CONTROL_TEST_DB not set"); return };
    let _sweep_guard = serialize_sweeps();
    let fx = build_fixture(&url, "idem").await;
    let creator = make_creator(&fx.state, "idem").await;
    let (_inv, stripe_in) = seed_finalized_invoice(&fx.state, creator, 7000).await;
    fx.mock.set_invoice(&stripe_in, MockInvoice {
        status: "paid".into(), amount_due: 0, amount_paid: 7000, total: 7000,
    });

    let stripe = real_client(&fx);
    let s1 = stripe_reconcile::tick_with(&fx.state, &stripe, now(), cfg()).await.expect("tick 1");
    let s2 = stripe_reconcile::tick_with(&fx.state, &stripe, now(), cfg()).await.expect("tick 2");

    assert_eq!(
        finding_count(&fx.state, "missed_invoice_payment", &stripe_in).await, 1,
        "two sweeps of the SAME drift yield exactly ONE finding (dedup_key idempotency)"
    );
    assert!(s1.findings_recorded >= 1, "first sweep recorded the finding");
    assert_eq!(s2.findings_recorded, 0, "second sweep recorded nothing new (deduped)");
}

// ===========================================================================
// (f) advisory-lock single-flight.
// ===========================================================================

#[compio::test]
async fn concurrent_tick_single_flights_under_advisory_lock() {
    let Some(url) = db_url() else { eprintln!("skip: CONTROL_TEST_DB not set"); return };
    let _sweep_guard = serialize_sweeps();
    let fx = build_fixture(&url, "single-flight").await;
    // Hold the reconcile advisory lock on a SEPARATE session (simulating another instance
    // mid-sweep), then drive a tick: it must find the lock held and no-op.
    let (holder, holder_conn) = compio_postgres::connect(&url, compio_postgres::NoTls).await.expect("holder conn");
    compio::runtime::spawn(async move { let _ = holder_conn.run().await; }).detach();
    // The key MUST match stripe_reconcile's constant (0x7a73_7265_636f_0001).
    let lock_key: i64 = 0x7a73_7265_636f_0001;
    let got = holder.query("SELECT pg_try_advisory_lock($1) AS l", &[&lock_key]).await.expect("lock");
    assert!(got[0].get::<_, bool>("l"), "the holder acquired the lock");

    let stripe = real_client(&fx);
    let summary = stripe_reconcile::tick_with(&fx.state, &stripe, now(), cfg()).await.expect("tick");
    assert!(!summary.ran, "a tick whose advisory lock is held elsewhere must single-flight to a no-op");
    assert_eq!(fx.mock.get_count("/v1/"), 0, "a single-flighted tick issues NO Stripe calls");

    // Release so the session is clean.
    let _ = holder.execute("SELECT pg_advisory_unlock($1)", &[&lock_key]).await;
}

/// Build the REAL `StripeClient` pointed at the mock (no shim on the wire path).
fn real_client(fx: &Fixture) -> zeroship_control::stripe_client::StripeClient {
    zeroship_control::stripe_client::StripeClient::new(SecretString::new("sk_test_mock".into()))
        .with_base_url(fx.state.stripe_base_url.clone())
}
