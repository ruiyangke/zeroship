//! Live-PG regression tests for Stripe webhook audit coverage.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};

use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    stripe_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .or_else(|_| std::env::var("AUTH_DB_URL"))
        .ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-stripe-webhook-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Fixture {
    async fn new(db_url: &str, label: &str) -> Self {
        // Default fixture: insecure_dev (empty webhook secret ⇒ signature
        // verification skipped) — matches the existing audit-coverage test.
        Self::new_with_secret(db_url, label, "", true).await
    }

    /// Build a fixture with an explicit webhook signing secret + `insecure_dev`
    /// flag. A non-empty secret with `insecure_dev=false` exercises the REAL
    /// signature-verify path (used by the forged-event test).
    async fn new_with_secret(
        db_url: &str,
        label: &str,
        webhook_secret: &str,
        insecure_dev: bool,
    ) -> Self {
        Self::new_full(db_url, label, webhook_secret, insecure_dev, "", "https://api.stripe.com").await
    }

    /// Fixture variant that wires a Stripe secret key + base URL — so the D2
    /// settlement-id fetch (`record_infra_payment` → `invoice_settlement_ids`) is
    /// actually exercised against a localhost mock.
    async fn new_with_stripe(
        db_url: &str,
        label: &str,
        stripe_secret_key: &str,
        stripe_base_url: &str,
    ) -> Self {
        Self::new_full(db_url, label, "", true, stripe_secret_key, stripe_base_url).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn new_full(
        db_url: &str,
        label: &str,
        webhook_secret: &str,
        insecure_dev: bool,
        stripe_secret_key: &str,
        stripe_base_url: &str,
    ) -> Self {
        let blob_root = tmpdir(&format!("blob-{label}"));
        let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
        let registry = Registry::new(db_url).await.expect("registry");
        let env_store =
            EnvStore::new(registry.clone(), "test-master-key", false).expect("env store");
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
            master_key: SecretString::new("test-master-key".to_string()),
            stripe_webhook_secret: SecretString::new(webhook_secret.to_string()),
            stripe_secret_key: SecretString::new(stripe_secret_key.to_string()),
            stripe_base_url: stripe_base_url.to_string(),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            insecure_dev,
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

        Self {
            state,
            blob_root,
            deploy_tmp_dir,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

/// A minimal mock-Stripe HTTP server that answers `GET /v1/invoices/{id}` with a
/// Basil expanded invoice (settling pi_/ch_ under `payments.data[].payment`). The
/// first `fail_n` GETs return HTTP 500 (a transient Stripe error → `StripeError::Api`
/// → the handler's settlement fetch fails closed); subsequent GETs return 200. Used
/// to exercise the D2 settlement fetch as the fallible LATER step inside
/// `record_infra_payment` (post-D3 the payout path no longer provides that step for
/// an infra invoice). Returns the base URL.
async fn start_flaky_invoice_mock(fail_n: u32) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let calls = Arc::new(AtomicU32::new(0));
    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let calls = Arc::clone(&calls);
            compio::runtime::spawn(async move {
                serve_flaky_conn(stream, calls, fail_n).await;
            })
            .detach();
        }
    })
    .detach();
    base_url
}

/// A mock-Stripe that answers EVERY `GET /v1/invoices/{id}` with the SAME fixed
/// settling `pi_`/`ch_` regardless of the invoice id — so two different internal
/// invoices resolve to the SAME globally-unique payment object (C2: a `pi_` reused
/// across a void+reissue). Returns the base URL.
async fn start_fixed_settlement_mock(pi: String, ch: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let (pi, ch) = (pi.clone(), ch.clone());
            compio::runtime::spawn(async move {
                serve_fixed_conn(stream, pi, ch).await;
            })
            .detach();
        }
    })
    .detach();
    base_url
}

async fn serve_fixed_conn(mut stream: TcpStream, pi: String, ch: String) {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        while let Some(end) = find_header_end(&acc) {
            let head = String::from_utf8_lossy(&acc[..end]).to_string();
            acc.drain(0..end + 4);
            let id = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|p| p.strip_prefix("/v1/invoices/"))
                .map(|s| s.split('?').next().unwrap_or("").to_string())
                .unwrap_or_default();
            let body = format!(
                r#"{{"id":"{id}","object":"invoice","status":"paid","payments":{{"object":"list","data":[{{"object":"invoice_payment","payment":{{"type":"payment_intent","payment_intent":{{"id":"{pi}","object":"payment_intent","latest_charge":"{ch}"}}}}}}]}}}}"#
            );
            if stream.write_all(http_resp(200, &body)).await.0.is_err() {
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

async fn serve_flaky_conn(mut stream: TcpStream, calls: Arc<AtomicU32>, fail_n: u32) {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        // Parse one request (headers terminated by CRLFCRLF; GETs carry no body).
        while let Some(end) = find_header_end(&acc) {
            let head = String::from_utf8_lossy(&acc[..end]).to_string();
            acc.drain(0..end + 4);
            let n = calls.fetch_add(1, Ordering::SeqCst);
            let resp = if n < fail_n {
                // Transient Stripe-style 5xx error body.
                let body = r#"{"error":{"type":"api_error","message":"transient"}}"#;
                http_resp(500, body)
            } else {
                // Extract the invoice id from the request line for echoing.
                let id = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|p| p.strip_prefix("/v1/invoices/"))
                    .map(|s| s.split('?').next().unwrap_or("").to_string())
                    .unwrap_or_default();
                // Derive UNIQUE settlement ids from the invoice id so concurrent /
                // sequential tests in the shared DB never collide on the
                // `billing_provider_refs (provider, ref_kind, external_id)` unique.
                let suffix = id.trim_start_matches("in_idem_");
                let pi = format!("pi_flaky_{suffix}");
                let ch = format!("ch_flaky_{suffix}");
                let body = format!(
                    r#"{{"id":"{id}","object":"invoice","status":"paid","payments":{{"object":"list","data":[{{"object":"invoice_payment","payment":{{"type":"payment_intent","payment_intent":{{"id":"{pi}","object":"payment_intent","latest_charge":"{ch}"}}}}}}]}}}}"#
                );
                http_resp(200, &body)
            };
            if stream.write_all(resp).await.0.is_err() {
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

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn http_resp(status: u16, body: &str) -> Vec<u8> {
    let reason = if status == 200 { "OK" } else { "Internal Server Error" };
    let mut resp = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body.as_bytes());
    resp
}

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(web::App::new().state($fx.state.clone()).service(
            // Mirror the production route's PayloadConfig so the handler's 413 body cap
            // is the effective boundary (the extractor's default 256KiB 400 would
            // otherwise win at the same threshold — see webhook_payload_config()).
            web::resource("/internal/webhooks/stripe")
                .state(stripe_handlers::webhook_payload_config())
                .route(web::post().to(stripe_handlers::webhook)),
        ))
        .await
    }};
}

#[compio::test]
async fn invoice_paid_webhook_records_app_audit_row() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "record-audit").await;
    let app = init_control!(fx);
    // `creator_accounts.creator_id` FKs to `users(id)` — seed a real user so
    // `link_account` satisfies the constraint on a freshly-migrated DB.
    let seed = side_conn(&db_url).await;
    let creator_id = make_user(&seed).await;
    fx.state
        .stripe_store
        .link_account(creator_id, "acct_webhookAudit1")
        .await
        .expect("link stripe account");

    let event_id = format!("evt_audit_{}", Uuid::new_v4().simple());
    let stripe_object_id = format!("in_audit_{}", Uuid::new_v4().simple());
    let body = json!({
        "id": event_id,
        "type": "invoice.paid",
        "created": 1_777_017_600i64,
        "data": {
            "object": {
                "id": stripe_object_id,
                "amount_paid": 1234,
                "application_fee_amount": 185,
                "currency": "usd",
                // M4: the settling account must be the claimed creator's own account.
                "on_behalf_of": "acct_webhookAudit1",
                "metadata": {
                    "creator_id": creator_id.to_string(),
                }
            }
        }
    });
    let req = test::TestRequest::post()
        .uri("/internal/webhooks/stripe")
        .header("content-type", "application/json")
        .set_payload(body.to_string())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let response_body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("webhook response json");
    assert_eq!(response_body["status"], "recorded");

    let (conn, conn_driver) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("registry conn");
    compio::runtime::spawn(async move {
        let _ = conn_driver.run().await;
    })
    .detach();
    let rows = conn
        .query(
            "SELECT resource, detail \
             FROM app_audit \
             WHERE creator_id = $1 AND action = 'record_payout' AND resource = $2",
            &[&creator_id, &event_id],
        )
        .await
        .expect("select payout audit");
    assert_eq!(rows.len(), 1);
    let detail: Value = rows[0].get("detail");
    assert_eq!(detail["creator_id"], creator_id.to_string());
    assert_eq!(detail["amount_cents"], 1234);
    assert_eq!(detail["stripe_event_id"], event_id);
    assert_eq!(detail["stripe_object_id"], stripe_object_id);
}

/// PR-1 (billing-ops gap #26): a paid INFRA invoice webhook appends a `charge`
/// `invoice_payments` row recording the cash actually collected, WITHOUT mutating
/// the finalized invoice; cash-collected = Σ(invoice_payments) reflects it.
///
/// FAITHFUL: drives the REAL `stripe_handlers::webhook` end to end (signature path,
/// JSON parse, dispatch, the infra branch's `record_infra_payment` → the REAL
/// `invoice_payments::append_charge`) against live PG. RED pre-fix: there is no
/// `invoice_payments` table and no webhook append, so the row never appears.
#[compio::test]
async fn infra_invoice_paid_appends_charge_payment_row() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "infra-payment").await;
    let app = init_control!(fx);
    let seed = side_conn(&db_url).await;
    let creator_id = make_user(&seed).await;
    seed.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) \
         ON CONFLICT (creator_id) DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");
    // Post-D3 an INFRA `invoice.paid` is fully handled by the infra branch and
    // returns 200 BEFORE the Stream-2 `record_payout` path — so no Connect link is
    // needed. (Kept linked here only to keep the fixture's account state realistic;
    // the assertion under test is the infra `invoice_payments` append, PR-1.)
    fx.state
        .stripe_store
        .link_account(creator_id, &format!("acct_{}", Uuid::new_v4().simple()))
        .await
        .expect("link stripe account");

    // Seed a FINALIZED internal invoice + the Stripe provider-invoice ref the
    // webhook maps `obj.id` back through. Period uniquified per-run.
    let period = {
        use chrono::Datelike;
        let now = chrono::Utc::now().date_naive();
        chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap()
    };
    let inv_id = zeroship_core::typed_id::new_invoice_id();
    seed.execute(
        "INSERT INTO zeroship.invoices \
           (id, creator_id, period, status, subtotal_cents, credit_cents, tax_cents, \
            total_cents, finalized_at) \
         VALUES ($1, $2, $3::date, 'finalized', 4500, 0, 0, 4500, NOW())",
        &[&inv_id, &creator_id, &period],
    )
    .await
    .expect("finalized invoice");
    let provider_invoice_id = format!("in_infra_{}", Uuid::new_v4().simple());
    seed.execute(
        "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'invoice', $2)",
        &[&inv_id, &provider_invoice_id],
    )
    .await
    .expect("provider ref");

    // POST a paid INFRA invoice webhook (invoice_kind=infra is the recovery/marker gate).
    let event_id = format!("evt_infra_{}", Uuid::new_v4().simple());
    let body = json!({
        "id": event_id,
        "type": "invoice.paid",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": provider_invoice_id,
            "amount_paid": 4500,
            "currency": "usd",
            "metadata": { "creator_id": creator_id.to_string(), "invoice_kind": "infra" }
        }}
    });
    let req = test::TestRequest::post()
        .uri("/internal/webhooks/stripe")
        .header("content-type", "application/json")
        .set_payload(body.to_string())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // A single `charge` row was appended for exactly the cash collected.
    let rows = seed
        .query(
            "SELECT amount_cents, kind, provider_ref FROM zeroship.invoice_payments \
             WHERE invoice_id = $1",
            &[&inv_id],
        )
        .await
        .expect("select payments");
    assert_eq!(rows.len(), 1, "exactly one charge row appended");
    assert_eq!(rows[0].get::<_, i64>("amount_cents"), 4500);
    assert_eq!(rows[0].get::<_, String>("kind"), "charge");
    assert_eq!(rows[0].get::<_, Option<String>>("provider_ref").as_deref(), Some(provider_invoice_id.as_str()));

    // cash-collected reflects it.
    let cash = zeroship_control::invoice_payments::cash_collected(&seed, &inv_id)
        .await
        .expect("cash_collected");
    assert_eq!(cash, 4500);

    // The FINALIZED invoice was NOT mutated.
    let inv = seed
        .query(
            "SELECT status, total_cents FROM zeroship.invoices WHERE id = $1",
            &[&inv_id],
        )
        .await
        .expect("read invoice");
    assert_eq!(inv[0].get::<_, String>("status"), "finalized");
    assert_eq!(inv[0].get::<_, i64>("total_cents"), 4500);
}

/// POST a webhook body to `$app`, optionally with a `stripe-signature` header.
/// A macro (not a fn) so it works on the opaque `Pipeline<…>` `init_service`
/// returns without naming its type.
macro_rules! post_webhook {
    ($app:expr, $body:expr, $sig:expr) => {{
        let mut req = test::TestRequest::post()
            .uri("/internal/webhooks/stripe")
            .header("content-type", "application/json");
        let sig: Option<&str> = $sig;
        if let Some(s) = sig {
            req = req.header("stripe-signature", s);
        }
        let req = req.set_payload($body.to_string()).to_request();
        test::call_service(&$app, req).await
    }};
}

// ─── PR-1 charge-append idempotency + fail-closed (gap #26 review) ───────────

/// First-of-this-month, the canonical infra-invoice period.
fn period_first_of_month() -> chrono::NaiveDate {
    use chrono::Datelike;
    let now = chrono::Utc::now().date_naive();
    chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap()
}

/// Seed a FINALIZED internal infra invoice + its Stripe provider-invoice ref, the
/// shape `record_infra_payment` maps `obj.id` back through. Returns
/// `(internal_invoice_id, provider_invoice_id)`.
async fn seed_finalized_infra_invoice(
    conn: &compio_postgres::Client,
    creator_id: Uuid,
    total: i64,
) -> (String, String) {
    let inv_id = zeroship_core::typed_id::new_invoice_id();
    conn.execute(
        "INSERT INTO zeroship.invoices \
           (id, creator_id, period, status, subtotal_cents, credit_cents, tax_cents, \
            total_cents, finalized_at) \
         VALUES ($1, $2, $3::date, 'finalized', $4, 0, 0, $4, NOW())",
        &[&inv_id, &creator_id, &period_first_of_month(), &total],
    )
    .await
    .expect("finalized invoice");
    let provider_invoice_id = format!("in_idem_{}", Uuid::new_v4().simple());
    conn.execute(
        "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'invoice', $2)",
        &[&inv_id, &provider_invoice_id],
    )
    .await
    .expect("provider ref");
    (inv_id, provider_invoice_id)
}

/// An infra `invoice.paid` body for `provider_invoice_id` under a chosen `event_id`.
fn infra_invoice_paid_body(
    event_id: &str,
    provider_invoice_id: &str,
    creator_id: Uuid,
    amount_paid: i64,
) -> String {
    json!({
        "id": event_id,
        "type": "invoice.paid",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": provider_invoice_id,
            "amount_paid": amount_paid,
            "currency": "usd",
            "metadata": { "creator_id": creator_id.to_string(), "invoice_kind": "infra" }
        }}
    })
    .to_string()
}

/// Count `charge` rows for an internal invoice.
async fn charge_row_count(conn: &compio_postgres::Client, invoice_id: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.invoice_payments \
         WHERE invoice_id = $1 AND kind = 'charge'",
        &[&invoice_id],
    )
    .await
    .expect("count charges")[0]
        .get::<_, i64>("n")
}

/// CRITICAL-1: TWO `invoice.paid` deliveries with DISTINCT `evt_id`s for the SAME
/// `provider_invoice_id` (Stripe re-finalize / uncollectible-then-paid redelivery
/// under a fresh event id) must append EXACTLY ONE `charge` row — cash_collected
/// equals the single payment amount, NOT double. Both events are distinct so the
/// `stripe_events_seen` gate does NOT dedup them; only the `provider_ref`
/// idempotency index keeps cash_collected honest.
///
/// RED pre-fix: the unconditional INSERT appends a second row → 2 charge rows →
/// cash_collected == 2× the amount → PR-3's over-refund cap inflates.
#[compio::test]
async fn distinct_events_same_invoice_append_one_charge_row() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "idem-distinct-evt").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");
    // Post-D3 the infra branch returns before the payout FK, so no Connect link is
    // needed for these infra deliveries to 200; this test isolates the PR-1 append
    // idempotency across two DISTINCT event ids for the same Stripe invoice.

    let (inv_id, provider_invoice_id) =
        seed_finalized_infra_invoice(&conn, creator_id, 4500).await;

    // Delivery #1 — fresh event id, full cash.
    let evt1 = format!("evt_idem1_{}", Uuid::new_v4().simple());
    let r1 = post_webhook!(
        app,
        infra_invoice_paid_body(&evt1, &provider_invoice_id, creator_id, 4500),
        None
    );
    assert_eq!(r1.status(), StatusCode::OK, "first delivery processed");

    // Delivery #2 — DIFFERENT event id, SAME Stripe invoice + cumulative amount.
    let evt2 = format!("evt_idem2_{}", Uuid::new_v4().simple());
    let r2 = post_webhook!(
        app,
        infra_invoice_paid_body(&evt2, &provider_invoice_id, creator_id, 4500),
        None
    );
    assert_eq!(r2.status(), StatusCode::OK, "second (distinct-evt) delivery processed");

    // Both events were claimed (distinct ids), yet exactly ONE charge row exists.
    assert_eq!(ledger_count(&conn, &evt1).await, 1, "evt1 claimed");
    assert_eq!(ledger_count(&conn, &evt2).await, 1, "evt2 claimed");
    assert_eq!(
        charge_row_count(&conn, &inv_id).await,
        1,
        "the same Stripe payment under two event ids must append exactly one charge row",
    );
    let cash = zeroship_control::invoice_payments::cash_collected(&conn, &inv_id)
        .await
        .expect("cash_collected");
    assert_eq!(cash, 4500, "cash_collected must be the single amount, NOT doubled");
}

/// CRITICAL-1 (same-event retry leg): the charge-row append runs BEFORE the fallible
/// D2 settlement-id FETCH, so "append committed, then a later step failed → event
/// UNCLAIMED → Stripe re-dispatches the SAME event id" must STILL leave exactly one
/// charge row across the retry.
///
/// Post-D3 the infra `invoice.paid` branch RETURNS (it no longer falls through to the
/// payout FK), so the fallible "later step" is the D2 settlement fetch
/// (`record_infra_payment` → `invoice_settlement_ids`). A flaky mock fails the FIRST
/// expanded `GET /v1/invoices` (HTTP 500 → `StripeError` → fail closed), then succeeds
/// on the retry. The body inlines NO pi_/ch_, so the fetch is forced.
///
/// RED pre-fix (PR-1): the first pass's unconditional INSERT already committed a row;
/// the retry's INSERT adds a SECOND → 2 charge rows → cash_collected doubles. The
/// `(invoice_id, provider_ref)` idempotency index keeps it to one.
#[compio::test]
async fn same_event_retry_after_later_failure_appends_one_charge_row() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    // Wire a flaky mock that fails the FIRST settlement-id GET, then succeeds.
    let base_url = start_flaky_invoice_mock(1).await;
    let fx = Fixture::new_with_stripe(&db_url, "idem-same-evt-retry", "sk_test_mock", &base_url).await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");

    let (inv_id, provider_invoice_id) =
        seed_finalized_infra_invoice(&conn, creator_id, 4500).await;

    let evt = format!("evt_idem_retry_{}", Uuid::new_v4().simple());
    // No inline pi_/ch_ → the handler MUST do the (flaky) settlement fetch.
    let body = infra_invoice_paid_body(&evt, &provider_invoice_id, creator_id, 4500);

    // First delivery: the charge row is appended, THEN the settlement-id fetch 500s
    // → 500, event NOT claimed.
    let r1 = post_webhook!(app, &body, None);
    assert_eq!(
        r1.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "settlement-fetch failure fails closed → retryable 5xx",
    );
    assert_eq!(ledger_count(&conn, &evt).await, 0, "event NOT claimed (will retry)");
    assert_eq!(
        charge_row_count(&conn, &inv_id).await,
        1,
        "the append committed on the first pass before the later (fetch) failure",
    );

    // Retry (SAME event id) — the mock now serves the invoice; append once → 200.
    let r2 = post_webhook!(app, &body, None);
    assert_eq!(r2.status(), StatusCode::OK, "retry processed (not lost)");
    assert_eq!(ledger_count(&conn, &evt).await, 1, "retry claimed once");

    // EXACTLY one charge row across the two deliveries.
    assert_eq!(
        charge_row_count(&conn, &inv_id).await,
        1,
        "the same-event retry appends exactly one charge row (idempotent)",
    );
    let cash = zeroship_control::invoice_payments::cash_collected(&conn, &inv_id)
        .await
        .expect("cash_collected");
    assert_eq!(cash, 4500, "cash_collected is the single amount, NOT doubled");

    // The retry recorded the settlement linkage fetched from the (now-healthy) mock.
    // (The mock derives a unique pi_ from the invoice id to avoid cross-test collisions.)
    let expected_pi = format!("pi_flaky_{}", provider_invoice_id.trim_start_matches("in_idem_"));
    let pi_refs = conn
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_provider_refs \
             WHERE invoice_id = $1 AND ref_kind = 'payment_intent' AND external_id = $2",
            &[&inv_id, &expected_pi],
        )
        .await
        .expect("count pi refs")[0]
        .get::<_, i64>("n");
    assert_eq!(pi_refs, 1, "the fetched pi_ linkage was recorded on retry (D2)");
}

/// MAJOR-2: a TRANSIENT `append_charge` failure must NOT be swallowed-then-claimed
/// (which would permanently DROP a cash row). The webhook must fail closed: the
/// event is left UNCLAIMED so Stripe retries. Here the Stripe object carries an
/// INVALID currency (`"USD"` — fails the `invoice_payments.currency` `^[a-z]{3}$`
/// CHECK), so the provider-ref lookup succeeds but `append_charge`'s INSERT raises
/// a constraint violation → `record_infra_payment` propagates the error → 500 →
/// event unclaimed. (A real-world stand-in for any transient DB error on the
/// append: a dropped conn, a lock timeout, etc.)
///
/// The object carries the `invoice_kind=infra` marker (so the infra branch + its
/// append run) but NO `metadata.creator_id` and NO `customer` — so the fall-through
/// Stream-2 `record_payout` path returns early (`missing_creator_id`) WITHOUT
/// touching the DB. The append is therefore the SOLE DB write, isolating its
/// failure: pre-fix the webhook would 200/`missing_creator_id` and claim the event;
/// post-fix the append error 500s before that.
///
/// RED pre-fix: `record_infra_payment` swallowed the error and returned `()`, the
/// webhook fell through to `missing_creator_id` (200), and `mark_event_processed`
/// CLAIMED the event — so the cash row was dropped AND never retried.
#[compio::test]
async fn append_failure_leaves_event_unclaimed_not_silently_dropped() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "fail-closed-append").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");

    // A REAL finalized invoice + provider ref so the lookup resolves; the append
    // itself fails on the invalid currency CHECK (stand-in for a transient append error).
    let (inv_id, provider_invoice_id) =
        seed_finalized_infra_invoice(&conn, creator_id, 4500).await;

    let evt = format!("evt_failclosed_{}", Uuid::new_v4().simple());
    // invoice_kind=infra marker WITHOUT creator metadata/customer → infra append
    // runs, then record_payout is skipped (missing_creator_id). Invalid currency
    // `"USD"` → invoice_payments.currency CHECK violation on the append INSERT.
    let body = json!({
        "id": evt,
        "type": "invoice.paid",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": provider_invoice_id,
            "amount_paid": 4500,
            "currency": "USD",
            "metadata": { "invoice_kind": "infra" }
        }}
    })
    .to_string();

    let r = post_webhook!(app, &body, None);
    assert_eq!(
        r.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "an append failure must fail the webhook closed (retryable 5xx)",
    );
    assert_eq!(
        ledger_count(&conn, &evt).await,
        0,
        "the event must NOT be claimed — so Stripe retries rather than the cash row being dropped",
    );
    // No charge row was committed (the INSERT itself failed).
    assert_eq!(charge_row_count(&conn, &inv_id).await, 0, "no partial charge row on failed append");
}

/// Count payout-ledger rows for a creator.
async fn payout_row_count(conn: &compio_postgres::Client, creator_id: Uuid) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.payouts WHERE creator_id = $1",
        &[&creator_id],
    )
    .await
    .expect("count payouts")[0]
        .get::<_, i64>("n")
}

/// D3 (real-Stripe regression): an INFRA `invoice.paid` for a creator with NO
/// `creator_accounts` (Connect) row must ACK 200 and NOT touch the Stream-2 payout
/// path. Pre-fix `dispatch_event` did not `return` after the infra branch, so it fell
/// through to `record_payout`, whose `payouts.creator_id → creator_accounts(creator_id)`
/// FK an infra-only creator cannot satisfy → 500 AFTER the infra writes committed
/// (non-atomic; the event never acked → Stripe retried forever — a poison loop).
///
/// RED pre-fix: HTTP 500 + the event left UNCLAIMED (poison). Post-fix: 200, the
/// charge row committed, and ZERO payout rows (the payout FK was never reached).
#[compio::test]
async fn infra_invoice_paid_without_connect_account_acks_200_no_payout() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "d3-infra-no-connect").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");
    // Deliberately NO link_account → no creator_accounts row (an infra-only creator).
    let (inv_id, provider_invoice_id) =
        seed_finalized_infra_invoice(&conn, creator_id, 4500).await;

    let evt = format!("evt_d3_{}", Uuid::new_v4().simple());
    let body = infra_invoice_paid_body(&evt, &provider_invoice_id, creator_id, 4500);
    let r = post_webhook!(app, &body, None);

    // ACKED 200 (no poison loop) — the infra branch returned before the payout FK.
    assert_eq!(r.status(), StatusCode::OK, "infra invoice.paid acks 200 without a Connect account");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "infra_recorded", "handled by the infra branch, not the payout path");
    assert_eq!(ledger_count(&conn, &evt).await, 1, "event claimed (acked) — Stripe will NOT retry");
    // The infra side-effect committed…
    assert_eq!(charge_row_count(&conn, &inv_id).await, 1, "the infra charge row was appended");
    // …and the payout path was NEVER reached (no FK violation, no payout row).
    assert_eq!(
        payout_row_count(&conn, creator_id).await,
        0,
        "an infra invoice.paid must NOT write a payout row (it returns before record_payout)",
    );
}

/// D3 (no-regression companion): a NON-infra `invoice.paid` (a real Connect-revenue
/// event — `metadata.creator_id` present, NO `invoice_kind=infra`) MUST still route to
/// the Stream-2 `record_payout` path and record a payout for a creator with a linked
/// Connect account. This proves the D3 `return` is scoped to infra invoices only and
/// did NOT break the legitimate Connect payout path.
#[compio::test]
async fn non_infra_invoice_paid_still_routes_to_payout() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "d3-connect-payout").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    // A real Connect creator: linked account so the payout FK is satisfied.
    let acct = format!("acct_{}", Uuid::new_v4().simple());
    fx.state
        .stripe_store
        .link_account(creator_id, &acct)
        .await
        .expect("link stripe account");

    // NON-infra invoice.paid: creator_id present, NO invoice_kind=infra marker.
    // M4: the settling account (on_behalf_of) is the creator's OWN account, so
    // attribution passes and the payout is credited.
    let evt = format!("evt_connect_{}", Uuid::new_v4().simple());
    let body = json!({
        "id": evt,
        "type": "invoice.paid",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": format!("in_connect_{}", Uuid::new_v4().simple()),
            "amount_paid": 1000,
            "application_fee_amount": 150,
            "currency": "usd",
            "on_behalf_of": acct,
            "metadata": { "creator_id": creator_id.to_string() }
        }}
    })
    .to_string();

    let r = post_webhook!(app, &body, None);
    assert_eq!(r.status(), StatusCode::OK, "connect-revenue invoice.paid processed");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "recorded", "routed through the Stream-2 record_payout path");
    assert_eq!(
        payout_row_count(&conn, creator_id).await,
        1,
        "a real Connect creator's invoice.paid still records a payout (D3 did not break this)",
    );
}

/// M4 (payout attribution): a Connect-revenue `invoice.paid` whose settling account
/// (`on_behalf_of`) is NOT the claimed `metadata.creator_id`'s own account must be
/// REJECTED — no payout credited. A forged creator id cannot steal another account's
/// revenue.
///
/// RED pre-fix: `record_payout` trusted `metadata.creator_id` with no ownership
/// check, so a payout was credited to the forged creator regardless of which account
/// actually settled the charge.
#[compio::test]
async fn payout_with_mismatched_settling_account_is_rejected() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "m4-attribution").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;

    // The CLAIMED creator owns acct A.
    let claimed_creator = make_user(&conn).await;
    let acct_a = format!("acct_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.link_account(claimed_creator, &acct_a).await.expect("link A");

    // But the charge settled on behalf of acct B (a DIFFERENT account).
    let other_creator = make_user(&conn).await;
    let acct_b = format!("acct_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.link_account(other_creator, &acct_b).await.expect("link B");

    let evt = format!("evt_m4_{}", Uuid::new_v4().simple());
    let body = json!({
        "id": evt,
        "type": "invoice.paid",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": format!("in_m4_{}", Uuid::new_v4().simple()),
            "amount_paid": 9999,
            "application_fee_amount": 100,
            "currency": "usd",
            // Settled on B, but metadata CLAIMS the (different) creator who owns A.
            "on_behalf_of": acct_b,
            "metadata": { "creator_id": claimed_creator.to_string() }
        }}
    })
    .to_string();

    let r = post_webhook!(app, &body, None);
    assert_eq!(r.status(), StatusCode::OK, "acked (no retry storm) but not credited");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "attribution_mismatch", "the mismatch is rejected, not credited");
    assert_eq!(
        payout_row_count(&conn, claimed_creator).await,
        0,
        "no payout credited to the claimed creator whose account did NOT settle the charge",
    );
    assert_eq!(
        payout_row_count(&conn, other_creator).await,
        0,
        "and certainly none mis-credited to the real settling account's creator",
    );
}

/// D2 (real-Stripe regression, webhook leg): a real `invoice.paid` payload carries NO
/// inline pi_/ch_ (Basil removed the top-level fields and the event isn't expanded).
/// `record_infra_payment` must FETCH the settlement ids via the expanded
/// `GET /v1/invoices` and record the `billing_provider_refs(payment_intent|charge)`
/// linkage from the FETCHED object — so a later real dispute can resolve back to us.
///
/// RED pre-fix: the handler read the ids off the (un-expandable) webhook payload, so a
/// payload WITHOUT them recorded NO linkage → a real dispute could never resolve.
#[compio::test]
async fn infra_invoice_paid_fetches_settlement_linkage_when_payload_omits_it() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    // A healthy mock (fail_n=0) that serves the expanded invoice with pi_flaky/ch_flaky.
    let base_url = start_flaky_invoice_mock(0).await;
    let fx = Fixture::new_with_stripe(&db_url, "d2-fetch-linkage", "sk_test_mock", &base_url).await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");
    let (inv_id, provider_invoice_id) =
        seed_finalized_infra_invoice(&conn, creator_id, 4500).await;

    // Payload OMITS pi_/ch_ entirely (the real-Stripe shape) — forces the fetch.
    let evt = format!("evt_d2_{}", Uuid::new_v4().simple());
    let body = infra_invoice_paid_body(&evt, &provider_invoice_id, creator_id, 4500);
    let r = post_webhook!(app, &body, None);
    assert_eq!(r.status(), StatusCode::OK, "infra invoice.paid processed");

    // The pi_/ch_ linkage was FETCHED and recorded (the dispute-resolution anchor).
    let refs = conn
        .query(
            "SELECT ref_kind, external_id FROM zeroship.billing_provider_refs \
             WHERE invoice_id = $1 AND ref_kind IN ('payment_intent','charge') ORDER BY ref_kind",
            &[&inv_id],
        )
        .await
        .expect("select refs");
    let pairs: Vec<(String, String)> = refs
        .iter()
        .map(|row| (row.get::<_, String>("ref_kind"), row.get::<_, String>("external_id")))
        .collect();
    // The mock derives unique pi_/ch_ from the invoice id (avoids cross-test collisions).
    let suffix = provider_invoice_id.trim_start_matches("in_idem_");
    assert!(
        pairs.contains(&("charge".to_string(), format!("ch_flaky_{suffix}"))),
        "ch_ linkage fetched + recorded; got {pairs:?}",
    );
    assert!(
        pairs.contains(&("payment_intent".to_string(), format!("pi_flaky_{suffix}"))),
        "pi_ linkage fetched + recorded; got {pairs:?}",
    );
}

/// C2 (webhook poison via the uncovered second unique): a settling `pi_`/`ch_`
/// already linked to invoice A, then seen for a DIFFERENT internal invoice B
/// (Stripe reuses a `pi_` across a void+reissue, which mints a new internal
/// invoice_id), must NOT poison the webhook. The linkage insert is tolerant of the
/// GLOBAL `UNIQUE(provider, ref_kind, external_id)`: invoice B's `invoice.paid`
/// acks 200 (the pi_ belongs to invoice A; it is an idempotent no-op), the event is
/// CLAIMED, and the cross-invoice ref is NOT created.
///
/// RED pre-fix: `record_payment_object_refs` did `ON CONFLICT (invoice_id, provider,
/// ref_kind) DO NOTHING`, which does NOT cover the global unique — so invoice B's
/// INSERT raised SQLSTATE 23505, propagated to a 500, left the event UNCLAIMED, and
/// Stripe retried forever (poison).
#[compio::test]
async fn settling_pi_reused_across_invoices_does_not_poison_webhook() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    // A mock that returns the SAME fixed pi_/ch_ for EVERY invoice — modelling a
    // settling object reused across the void+reissue. Uniquified per run so the
    // persistent test DB's global-unique constraint never collides across runs.
    let suffix = Uuid::new_v4().simple().to_string();
    let shared_pi = format!("pi_shared_c2_{suffix}");
    let shared_ch = format!("ch_shared_c2_{suffix}");
    let base_url = start_fixed_settlement_mock(shared_pi.clone(), shared_ch.clone()).await;
    let fx = Fixture::new_with_stripe(&db_url, "c2-poison", "sk_test_mock", &base_url).await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");

    // Invoice A: finalized, with the shared pi_ ALREADY linked (the prior settlement).
    let (inv_a, _provider_a) = seed_finalized_infra_invoice(&conn, creator_id, 4500).await;
    conn.execute(
        "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'payment_intent', $2)",
        &[&inv_a, &shared_pi],
    )
    .await
    .expect("pre-link pi_ to invoice A");

    // Invoice B: a DIFFERENT internal invoice (the reissue), whose invoice.paid
    // fetches the SAME shared pi_ from the mock. Seeded in a DISTINCT period so the
    // (creator, period) partial-unique index does not block the second invoice (the
    // void+reissue scenario the C2 fix targets is about the SHARED pi_, not the period).
    let inv_b = zeroship_core::typed_id::new_invoice_id();
    let period_b = {
        use chrono::Datelike;
        let now = chrono::Utc::now().date_naive();
        // The month before this one — guaranteed distinct from period_first_of_month().
        let (y, m) = if now.month() == 1 { (now.year() - 1, 12) } else { (now.year(), now.month() - 1) };
        chrono::NaiveDate::from_ymd_opt(y, m, 1).unwrap()
    };
    conn.execute(
        "INSERT INTO zeroship.invoices \
           (id, creator_id, period, status, subtotal_cents, credit_cents, tax_cents, total_cents, finalized_at) \
         VALUES ($1, $2, $3::date, 'finalized', 4500, 0, 0, 4500, NOW())",
        &[&inv_b, &creator_id, &period_b],
    )
    .await
    .expect("finalized invoice B");
    let provider_b = format!("in_idem_{}", Uuid::new_v4().simple());
    conn.execute(
        "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'invoice', $2)",
        &[&inv_b, &provider_b],
    )
    .await
    .expect("provider ref B");
    let evt = format!("evt_c2_{}", Uuid::new_v4().simple());
    // Body omits inline pi_/ch_ → the handler MUST fetch (gets the shared pi_).
    let body = infra_invoice_paid_body(&evt, &provider_b, creator_id, 4500);

    let r = post_webhook!(app, &body, None);
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "invoice B's invoice.paid must ACK 200 — the reused pi_ must not poison the webhook",
    );
    assert_eq!(ledger_count(&conn, &evt).await, 1, "event CLAIMED — Stripe will NOT retry (no poison loop)");

    // Invoice B's charge row was appended (the cash is still recorded)…
    assert_eq!(charge_row_count(&conn, &inv_b).await, 1, "invoice B charge row appended");
    // …but the shared pi_ ref still belongs ONLY to invoice A (the first settlement).
    let pi_rows = conn
        .query(
            "SELECT invoice_id FROM zeroship.billing_provider_refs \
             WHERE provider = 'stripe' AND ref_kind = 'payment_intent' AND external_id = $1",
            &[&shared_pi],
        )
        .await
        .expect("select pi refs");
    assert_eq!(pi_rows.len(), 1, "exactly one row maps the globally-unique pi_");
    assert_eq!(
        pi_rows[0].get::<_, String>("invoice_id"),
        inv_a,
        "the reused pi_ stays linked to the FIRST settlement (invoice A), never re-pointed to B",
    );
}

/// M1 (dunning must fail-closed): an error from `record_payment_failed` during an
/// `invoice.payment_failed` webhook must FAIL CLOSED — return 5xx and leave the
/// event UNCLAIMED so Stripe retries. Otherwise the event is marked processed,
/// Stripe never redelivers, and the creator never enters dunning (consuming free
/// infra on a dead card).
///
/// We force the error with a `metadata.creator_id` that is a well-formed UUID but
/// NOT a real `users` row: `record_payment_failed`'s parent-first
/// `INSERT INTO creator_billing (creator_id)` FK-violates `users(id)` → Err.
///
/// RED pre-fix: the handler logged the error and fell through to 200; the event was
/// CLAIMED (1 ledger row) and dunning never armed.
#[compio::test]
async fn payment_failed_record_error_fails_closed_unclaimed() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "m1-dunning-failclosed").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;

    // A creator_id that is NOT a real user → the parent-first creator_billing insert
    // FK-violates → record_payment_failed errors.
    let bogus_creator = Uuid::new_v4();
    let evt = format!("evt_pf_failclosed_{}", Uuid::new_v4().simple());
    let body = json!({
        "id": evt,
        "type": "invoice.payment_failed",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": format!("in_pf_{}", Uuid::new_v4().simple()),
            "currency": "usd",
            "metadata": { "creator_id": bogus_creator.to_string(), "invoice_kind": "infra" }
        }}
    })
    .to_string();

    let r = post_webhook!(app, &body, None);
    assert_eq!(
        r.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a record_payment_failed error must fail the webhook closed (retryable 5xx)",
    );
    assert_eq!(
        ledger_count(&conn, &evt).await,
        0,
        "the event must NOT be claimed — Stripe retries so the creator still enters dunning",
    );
}

/// M2 (the money hole): an `account.updated` flipping `charges_enabled=false`
/// (Stripe risk/KYC hold) must update the CACHED flag so the `connect_checkout`
/// gate (which reads `creator_accounts.charges_enabled`) now blocks the account.
///
/// RED pre-fix: `account.updated` fell into the silent `_ => ignored` arm — the
/// cached `charges_enabled` stayed `true`, and a disabled account kept passing the
/// checkout gate.
#[compio::test]
async fn account_updated_disables_cached_charges_flag() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "m2-account-updated").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    let acct = format!("acct_{}", Uuid::new_v4().simple());
    // Link + mark the account fully enabled (the state before the risk hold).
    fx.state.stripe_store.link_account(creator_id, &acct).await.expect("link");
    fx.state
        .stripe_store
        .set_account_flags(creator_id, &acct, true, true, true)
        .await
        .expect("enable flags");
    // Sanity: the gate would pass right now.
    assert!(
        fx.state.stripe_store.get_account(creator_id).await.unwrap().unwrap().charges_enabled,
        "precondition: account is charges_enabled before the risk hold",
    );

    // account.updated with charges_enabled=false (the risk/KYC disable).
    let evt = format!("evt_acct_{}", Uuid::new_v4().simple());
    let body = json!({
        "id": evt,
        "type": "account.updated",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": acct,
            "object": "account",
            "charges_enabled": false,
            "payouts_enabled": false,
            "details_submitted": true
        }}
    })
    .to_string();
    let r = post_webhook!(app, &body, None);
    assert_eq!(r.status(), StatusCode::OK, "account.updated processed");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "account_flags_updated");
    assert_eq!(ledger_count(&conn, &evt).await, 1, "event claimed");

    // The CACHED flag the connect_checkout gate reads is now FALSE → gate blocks.
    let acct_row = fx.state.stripe_store.get_account(creator_id).await.unwrap().unwrap();
    assert!(
        !acct_row.charges_enabled,
        "account.updated must flip the cached charges_enabled to false so checkout is blocked",
    );
}

/// M2 (no double-debit): a `charge.dispute.funds_withdrawn` event must NOT append a
/// second `dispute_debit` `invoice_payments` row — the cash movement is owned by the
/// dispute LIFECYCLE handler (`charge.dispute.created`). The funds event is
/// audit-only, so `Σ(invoice_payments)` is debited EXACTLY ONCE.
///
/// RED pre-fix: `funds_withdrawn` fell into the silent `_ => ignored` arm, which
/// (correctly) did nothing — but there was no explicit guard / test pinning the
/// single-source-of-truth invariant; this test makes the no-double-debit explicit
/// and guards against a future handler being wired to BOTH rails.
#[compio::test]
async fn dispute_funds_event_does_not_double_debit() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "m2-funds-nodouble").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");

    // A finalized invoice with a charge collected + the ch_ linkage the dispute resolves through.
    let (inv_id, _provider) = seed_finalized_infra_invoice(&conn, creator_id, 4500).await;
    append_charge_via_helper(&conn, &inv_id, 4500).await;
    let ch = format!("ch_funds_{}", Uuid::new_v4().simple());
    conn.execute(
        "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'charge', $2)",
        &[&inv_id, &ch],
    )
    .await
    .expect("ch ref");

    // Lifecycle: charge.dispute.created debits once.
    let du = format!("du_{}", Uuid::new_v4().simple());
    let created_evt = format!("evt_dispute_created_{}", Uuid::new_v4().simple());
    let created = json!({
        "id": created_evt,
        "type": "charge.dispute.created",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": du, "amount": 4500, "currency": "usd", "reason": "fraudulent",
            "charge": ch, "status": "needs_response"
        }}
    })
    .to_string();
    let r1 = post_webhook!(app, &created, None);
    assert_eq!(r1.status(), StatusCode::OK, "dispute.created recorded");
    let debits_after_created = dispute_debit_count(&conn, &inv_id).await;
    assert_eq!(debits_after_created, 1, "exactly one dispute_debit from the lifecycle handler");

    // Funds event: must NOT add a second debit.
    let funds_evt = format!("evt_funds_{}", Uuid::new_v4().simple());
    let funds = json!({
        "id": funds_evt,
        "type": "charge.dispute.funds_withdrawn",
        "created": 1_777_017_700i64,
        "data": { "object": {
            "id": du, "amount": 4500, "currency": "usd", "charge": ch
        }}
    })
    .to_string();
    let r2 = post_webhook!(app, &funds, None);
    assert_eq!(r2.status(), StatusCode::OK, "funds event acked");
    let b: Value = serde_json::from_slice(&test::read_body(r2).await).unwrap();
    assert_eq!(b["status"], "funds_event_audited");
    assert_eq!(
        dispute_debit_count(&conn, &inv_id).await,
        1,
        "the funds event must NOT add a second dispute_debit (single source of truth)",
    );
}

/// Append a charge row through the REAL helper (keeps cash_collected honest).
async fn append_charge_via_helper(conn: &compio_postgres::Client, inv_id: &str, amount: i64) {
    zeroship_control::invoice_payments::append_charge(
        conn,
        inv_id,
        amount,
        "usd",
        Some(&format!("in_paid_{}", Uuid::new_v4().simple())),
    )
    .await
    .expect("append charge");
}

/// Count `dispute_debit` rows for an internal invoice.
async fn dispute_debit_count(conn: &compio_postgres::Client, invoice_id: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.invoice_payments \
         WHERE invoice_id = $1 AND kind = 'dispute_debit'",
        &[&invoice_id],
    )
    .await
    .expect("count debits")[0]
        .get::<_, i64>("n")
}

/// M3 (dedup concurrency): `lock_event` takes a SESSION advisory lock keyed on the
/// event id, so a SECOND connection's `pg_try_advisory_lock` on the SAME key FAILS
/// while it is held — the same-event redeliveries serialize. Mirrors the PR-2
/// consume-lock test (`issue_refund_takes_per_creator_advisory_lock`).
///
/// RED pre-fix: there was no lock around the check-then-act, so the try-lock on the
/// same key would succeed (no serialization).
#[compio::test]
async fn lock_event_serializes_same_event() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "m3-event-lock").await;
    let probe = side_conn(&db_url).await;
    let event_id = format!("evt_lock_{}", Uuid::new_v4().simple());

    // Take the per-event session advisory lock (held on the returned connection).
    let lock_conn = fx.state.stripe_store.lock_event(&event_id).await.expect("lock");

    // A try-lock on the SAME key from a DIFFERENT connection must FAIL (lock held).
    let got: bool = probe
        .query(
            "SELECT pg_try_advisory_lock(hashtext($1::text)::bigint) AS got",
            &[&event_id],
        )
        .await
        .expect("try-lock while held")[0]
        .get("got");
    assert!(
        !got,
        "while lock_event holds the per-event lock, a concurrent try-lock on the same key must fail (serialized)",
    );
    // (If the try-lock had somehow succeeded, release it so the probe conn is clean.)
    if got {
        let _ = probe
            .execute("SELECT pg_advisory_unlock(hashtext($1::text)::bigint)", &[&event_id])
            .await;
    }

    // Release the held lock; the same key is now acquirable.
    zeroship_control::StripeStore::unlock_event(&lock_conn, &event_id).await;
    let got2: bool = probe
        .query(
            "SELECT pg_try_advisory_lock(hashtext($1::text)::bigint) AS got",
            &[&event_id],
        )
        .await
        .expect("try-lock after release")[0]
        .get("got");
    assert!(got2, "after unlock_event the per-event lock is free again");
    let _ = probe
        .execute("SELECT pg_advisory_unlock(hashtext($1::text)::bigint)", &[&event_id])
        .await;
    drop(lock_conn);
}

// ─── G6 replay-dedup ledger tests ──────────────────────────────────────────

/// Open a side connection for seeding users + reading the dedup ledger / audit.
async fn side_conn(db_url: &str) -> compio_postgres::Client {
    let (conn, driver) = compio_postgres::connect(db_url, compio_postgres::NoTls)
        .await
        .expect("side connect");
    compio::runtime::spawn(async move {
        let _ = driver.run().await;
    })
    .detach();
    conn
}

/// Insert a fresh `zeroship.users` row (FK target of `creator_billing`) and
/// return its id. Unique email per call.
async fn make_user(conn: &compio_postgres::Client) -> Uuid {
    let rows = conn
        .query(
            "INSERT INTO zeroship.users (email, name) \
             VALUES ($1, 'g6-dedup-test') RETURNING id",
            &[&format!("g6-{}@test.invalid", Uuid::new_v4().simple())],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

/// Count rows in the dedup ledger for a given event-id (0 ⇒ not yet claimed).
async fn ledger_count(conn: &compio_postgres::Client, event_id: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.stripe_events_seen WHERE event_id = $1",
        &[&event_id],
    )
    .await
    .expect("count ledger")[0]
        .get::<_, i64>("n")
}

/// Count `setup_intent_succeeded` audit rows for a creator+event — the proof
/// the handler ran (one row per actual processing).
async fn setup_audit_count(conn: &compio_postgres::Client, creator_id: Uuid, event_id: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM app_audit \
         WHERE creator_id = $1 AND action = 'setup_intent_succeeded' AND resource = $2",
        &[&creator_id, &event_id],
    )
    .await
    .expect("count audit")[0]
        .get::<_, i64>("n")
}

fn setup_intent_body(event_id: &str, creator_id: Uuid) -> String {
    json!({
        "id": event_id,
        "type": "setup_intent.succeeded",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": format!("seti_{}", Uuid::new_v4().simple()),
            "metadata": { "creator_id": creator_id.to_string() }
        }}
    })
    .to_string()
}

/// A re-delivered IDENTICAL event is deduped by the ledger — the handler is NOT
/// re-run (no second audit row) and the redelivery is 200-acked. WITHOUT the
/// `stripe_events_seen` ledger this fails: `setup_intent.succeeded` has no
/// payouts-table dedup of its own, so the handler runs twice (two audit rows).
#[compio::test]
async fn redelivered_event_is_deduped_handler_not_rerun() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "dedup-replay").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    let event_id = format!("evt_dedup_{}", Uuid::new_v4().simple());
    let body = setup_intent_body(&event_id, creator_id);

    // First delivery — processed, ledger claimed, one audit row.
    let r1 = post_webhook!(app, &body, None);
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value = serde_json::from_slice(&test::read_body(r1).await).unwrap();
    assert_eq!(b1["status"], "default_pm_set");
    assert_eq!(ledger_count(&conn, &event_id).await, 1, "event claimed after success");
    assert_eq!(setup_audit_count(&conn, creator_id, &event_id).await, 1);

    // Re-delivery of the EXACT same event — deduped, handler NOT re-run.
    let r2 = post_webhook!(app, &body, None);
    assert_eq!(r2.status(), StatusCode::OK);
    let b2: Value = serde_json::from_slice(&test::read_body(r2).await).unwrap();
    assert_eq!(b2["status"], "duplicate", "redelivery acked as duplicate");
    assert_eq!(ledger_count(&conn, &event_id).await, 1, "no second ledger row");
    assert_eq!(
        setup_audit_count(&conn, creator_id, &event_id).await,
        1,
        "handler did NOT re-run on redelivery — exactly-once effective"
    );
}

/// A FORGED / unsigned event is rejected by signature verification BEFORE the
/// dedup ledger is touched: it is neither claimed (no ledger row) nor processed
/// (no audit row). Proves sig-verify-FIRST ordering.
#[compio::test]
async fn forged_event_rejected_before_ledger_claim() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    // Real secret + verification ON.
    let fx = Fixture::new_with_secret(&db_url, "dedup-forged", "whsec_test_g6", false).await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    let event_id = format!("evt_forged_{}", Uuid::new_v4().simple());
    let body = setup_intent_body(&event_id, creator_id);

    // Bogus signature header — verification must reject.
    let r = post_webhook!(app, &body, Some("t=1777017600,v1=deadbeef"));
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "forged event rejected");
    assert_eq!(
        ledger_count(&conn, &event_id).await,
        0,
        "forged event NEVER claimed in the dedup ledger"
    );
    assert_eq!(
        setup_audit_count(&conn, creator_id, &event_id).await,
        0,
        "forged event NEVER processed"
    );
}

/// A handler that FAILS (non-2xx) does NOT claim the event — so Stripe's retry
/// re-processes it (no lost event). Here the first delivery names a creator_id
/// with no `users` row ⇒ `set_default_pm`'s FK insert errors ⇒ 500, unclaimed.
/// The retry (after the user exists) succeeds and is then recorded once.
#[compio::test]
async fn handler_failure_is_retried_not_lost() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "dedup-retry").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;

    // A creator_id that is NOT a real user → set_default_pm FK insert fails.
    let creator_id = Uuid::new_v4();
    let event_id = format!("evt_retry_{}", Uuid::new_v4().simple());
    let body = setup_intent_body(&event_id, creator_id);

    // First delivery: handler errors (FK violation) → non-2xx, NOT claimed.
    let r1 = post_webhook!(app, &body, None);
    assert_eq!(
        r1.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "handler failed (FK) → retryable 5xx"
    );
    assert_eq!(
        ledger_count(&conn, &event_id).await,
        0,
        "failed handler did NOT claim the event — Stripe will retry"
    );

    // Now the user exists (creator finished signup before the retry lands).
    conn.execute(
        "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2, 'g6-retry')",
        &[&creator_id, &format!("g6-retry-{}@test.invalid", creator_id.simple())],
    )
    .await
    .expect("insert user with fixed id");

    // Retry (same event_id) — now succeeds and IS recorded exactly once.
    let r2 = post_webhook!(app, &body, None);
    assert_eq!(r2.status(), StatusCode::OK, "retry processed (not lost)");
    let b2: Value = serde_json::from_slice(&test::read_body(r2).await).unwrap();
    assert_eq!(b2["status"], "default_pm_set");
    assert_eq!(ledger_count(&conn, &event_id).await, 1, "retry recorded once");
    assert_eq!(setup_audit_count(&conn, creator_id, &event_id).await, 1);
}

// ════════════════════════════════════════════════════════════════════════════
// Webhook follow-ups (0054): charge.refund.updated / payout.failed /
// payment_intent.payment_failed — the 3 previously-deferred handlers.
// ════════════════════════════════════════════════════════════════════════════

/// A fresh OWNED Postgres connection (mutable) — `refund::issue_refund` opens a txn for
/// its claim, so it needs `&mut`.
async fn owned_conn(db_url: &str) -> compio_postgres::Client {
    let (conn, driver) = compio_postgres::connect(db_url, compio_postgres::NoTls)
        .await
        .expect("owned connect");
    compio::runtime::spawn(async move {
        let _ = driver.run().await;
    })
    .detach();
    conn
}

/// Credit balance for a creator = SUM(credit_ledger.amount_cents).
async fn credit_balance(conn: &compio_postgres::Client, creator_id: Uuid) -> i64 {
    conn.query(
        "SELECT COALESCE(SUM(amount_cents),0)::bigint AS b FROM zeroship.credit_ledger WHERE creator_id = $1",
        &[&creator_id],
    )
    .await
    .expect("credit balance")[0]
        .get::<_, i64>("b")
}

/// The status of a refund row.
async fn refund_status(conn: &compio_postgres::Client, refund_id: &str) -> String {
    conn.query(
        "SELECT status::text AS s FROM zeroship.refunds WHERE id = $1",
        &[&refund_id],
    )
    .await
    .expect("refund status")[0]
        .get::<_, String>("s")
}

/// Count `refund_clawback` credit entries for a refund's note.
async fn clawback_count(conn: &compio_postgres::Client, refund_id: &str) -> i64 {
    let note = zeroship_control::refund::refund_clawback_note(refund_id);
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.credit_ledger \
         WHERE kind = 'refund_clawback' AND note = $1",
        &[&note],
    )
    .await
    .expect("clawback count")[0]
        .get::<_, i64>("n")
}

/// charge.refund.updated body for a re_… that transitioned to `status`.
fn refund_updated_body(event_id: &str, re_id: &str, status: &str) -> String {
    json!({
        "id": event_id,
        "type": "charge.refund.updated",
        "created": 1_777_017_700i64,
        "data": { "object": {
            "id": re_id,
            "object": "refund",
            "status": status
        }}
    })
    .to_string()
}

/// MONEY-CRITICAL (charge.refund.updated, CASH leg): a CASH refund whose Stripe `Refund`
/// later FAILS must be marked `failed` so the over-refund cap STOPS counting it — the
/// creator can re-refund the same cash. A redelivery is a no-op.
///
/// RED pre-fix: the deferred arm left the refund `issued`, so the cash stayed
/// permanently "refunded" and a re-refund was blocked by the over-refund cap.
#[compio::test]
async fn refund_updated_failed_cash_refund_frees_the_cap_idempotently() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "refund-updated-cash").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");

    // Finalized invoice + cash collected + the in_… charge provider_ref so the cash refund
    // resolves a money object.
    let (inv_id, provider_invoice_id) = seed_finalized_infra_invoice(&conn, creator_id, 5000).await;
    zeroship_control::invoice_payments::append_charge(
        &conn, &inv_id, 5000, "usd", Some(&provider_invoice_id),
    )
    .await
    .expect("append charge");

    // Issue a CASH refund for the FULL cash → the Native provider mints a
    // `re_native_<idem>` recorded as a refund_provider_refs(ref_kind='refund') row.
    let mut oc = owned_conn(&db_url).await;
    let provider = zeroship_control::refund::NativeRefundProvider;
    let idem = format!("refkey_cash_{}", Uuid::new_v4().simple());
    let outcome = zeroship_control::refund::issue_refund(
        &mut oc, &provider, &inv_id, 5000, 5000, 0,
        zeroship_control::refund::RefundDestination::Cash, Some("test"), &idem,
    )
    .await
    .expect("issue cash refund");
    let (refund_id, re_id) = match outcome {
        zeroship_control::refund::RefundOutcome::Issued { refund_id, provider_ref } => {
            (refund_id, provider_ref.expect("cash refund has a re_…"))
        }
        other => panic!("expected Issued, got {other:?}"),
    };
    drop(oc);
    assert_eq!(refund_status(&conn, &refund_id).await, "issued", "refund issued");

    // Sanity: a SECOND full cash refund is now BLOCKED (the cap is consumed).
    {
        let mut oc2 = owned_conn(&db_url).await;
        let blocked = zeroship_control::refund::issue_refund(
            &mut oc2, &provider, &inv_id, 5000, 5000, 0,
            zeroship_control::refund::RefundDestination::Cash, None,
            &format!("refkey_blocked_{}", Uuid::new_v4().simple()),
        )
        .await
        .expect("second refund attempt");
        assert!(
            matches!(blocked, zeroship_control::refund::RefundOutcome::OverRefund(_)),
            "while the first refund is issued, a second full refund is over-cap; got {blocked:?}",
        );
        drop(oc2);
    }

    // charge.refund.updated → status=failed: the bank rejected the credit.
    let evt = format!("evt_refundfail_{}", Uuid::new_v4().simple());
    let r = post_webhook!(app, refund_updated_body(&evt, &re_id, "failed"), None);
    assert_eq!(r.status(), StatusCode::OK, "refund.updated processed");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "refund_reversed");
    assert_eq!(refund_status(&conn, &refund_id).await, "failed", "refund flipped to failed");
    assert_eq!(ledger_count(&conn, &evt).await, 1, "event claimed");

    // The cap is FREED: a fresh full cash refund now SUCCEEDS (the failed one no longer counts).
    {
        let mut oc3 = owned_conn(&db_url).await;
        let re_refund = zeroship_control::refund::issue_refund(
            &mut oc3, &provider, &inv_id, 5000, 5000, 0,
            zeroship_control::refund::RefundDestination::Cash, None,
            &format!("refkey_rerefund_{}", Uuid::new_v4().simple()),
        )
        .await
        .expect("re-refund after the failed one");
        assert!(
            matches!(re_refund, zeroship_control::refund::RefundOutcome::Issued { .. }),
            "a failed refund frees the cap so the cash can be re-refunded; got {re_refund:?}",
        );
        drop(oc3);
    }

    // Redelivery of the SAME charge.refund.updated is a no-op (refund already failed).
    let evt2 = format!("evt_refundfail2_{}", Uuid::new_v4().simple());
    let r2 = post_webhook!(app, refund_updated_body(&evt2, &re_id, "failed"), None);
    assert_eq!(r2.status(), StatusCode::OK, "redelivery processed");
    let b2: Value = serde_json::from_slice(&test::read_body(r2).await).unwrap();
    assert_eq!(b2["status"], "refund_already_reversed", "no double-reversal");
}

/// MONEY-CRITICAL (charge.refund.updated, credit clawback via a directly-seeded re_…):
/// a credit-destination refund whose Refund later FAILS claws back the minted credit,
/// conserving the balance; a redelivery is a no-op. We seed the re_… linkage directly
/// (a credit refund carries none natively) to exercise the clawback path end-to-end.
#[compio::test]
async fn refund_updated_failed_credit_claws_back_grant() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "refund-updated-claw").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator_id],
    )
    .await
    .expect("creator_billing");

    let (inv_id, _provider) = seed_finalized_infra_invoice(&conn, creator_id, 5000).await;
    append_charge_via_helper(&conn, &inv_id, 5000).await;

    let mut oc = owned_conn(&db_url).await;
    let provider = zeroship_control::refund::NativeRefundProvider;
    let idem = format!("refkey_claw_{}", Uuid::new_v4().simple());
    let outcome = zeroship_control::refund::issue_refund(
        &mut oc, &provider, &inv_id, 2000, 2000, 0,
        zeroship_control::refund::RefundDestination::Credit, None, &idem,
    )
    .await
    .expect("issue credit refund");
    let refund_id = match outcome {
        zeroship_control::refund::RefundOutcome::Issued { refund_id, .. } => refund_id,
        other => panic!("expected Issued, got {other:?}"),
    };
    let balance_before = credit_balance(&conn, creator_id).await;
    assert_eq!(balance_before, 2000, "credit minted");

    // Seed the re_… cash ref the failed-refund webhook resolves through. (Stripe does
    // surface a re_… on a customer-balance refund's charge.refund.updated; a credit refund
    // in our model would normally have none — we seed it to exercise the clawback path.)
    let re_id = format!("re_seed_{}", Uuid::new_v4().simple());
    conn.execute(
        "INSERT INTO zeroship.refund_provider_refs (refund_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'refund', $2)",
        &[&refund_id, &re_id],
    )
    .await
    .expect("seed re_ ref");
    drop(oc);

    // charge.refund.updated → failed: claw back the credit.
    let evt = format!("evt_claw_{}", Uuid::new_v4().simple());
    let r = post_webhook!(app, refund_updated_body(&evt, &re_id, "failed"), None);
    assert_eq!(r.status(), StatusCode::OK, "refund.updated processed");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "refund_reversed");
    assert_eq!(b["credit_clawed_back"], true, "the minted credit was clawed back");

    assert_eq!(refund_status(&conn, &refund_id).await, "failed", "refund failed");
    assert_eq!(clawback_count(&conn, &refund_id).await, 1, "exactly one refund_clawback entry");
    assert_eq!(
        credit_balance(&conn, creator_id).await,
        0,
        "balance conserved: +2000 grant − 2000 clawback = 0 (the phantom credit is gone)",
    );

    // Redelivery: no second clawback, balance unchanged.
    let evt2 = format!("evt_claw2_{}", Uuid::new_v4().simple());
    let r2 = post_webhook!(app, refund_updated_body(&evt2, &re_id, "failed"), None);
    assert_eq!(r2.status(), StatusCode::OK);
    let b2: Value = serde_json::from_slice(&test::read_body(r2).await).unwrap();
    assert_eq!(b2["status"], "refund_already_reversed", "no double-reversal");
    assert_eq!(clawback_count(&conn, &refund_id).await, 1, "still exactly one clawback");
    assert_eq!(credit_balance(&conn, creator_id).await, 0, "balance still conserved");
}

/// A `charge.refund.updated` with a NON-terminal status (e.g. `succeeded`) is a benign
/// no-op — it must NOT reverse a healthy refund.
#[compio::test]
async fn refund_updated_succeeded_is_noop() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "refund-updated-ok").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let evt = format!("evt_refok_{}", Uuid::new_v4().simple());
    let re_id = format!("re_ok_{}", Uuid::new_v4().simple());
    let r = post_webhook!(app, refund_updated_body(&evt, &re_id, "succeeded"), None);
    assert_eq!(r.status(), StatusCode::OK);
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "refund_update_noop", "a non-failure update is a no-op");
    assert_eq!(ledger_count(&conn, &evt).await, 1, "event still claimed (acked)");
}

/// Count payout_failures rows for a creator.
async fn payout_failure_count(conn: &compio_postgres::Client, creator_id: Uuid) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.payout_failures WHERE creator_id = $1",
        &[&creator_id],
    )
    .await
    .expect("count payout failures")[0]
        .get::<_, i64>("n")
}

/// payout.failed body: a Connect event (top-level `account`) for a `po_…` payout.
fn payout_failed_body(event_id: &str, po_id: &str, account: &str, amount: i64) -> String {
    json!({
        "id": event_id,
        "type": "payout.failed",
        "created": 1_777_017_800i64,
        "account": account,
        "data": { "object": {
            "id": po_id,
            "object": "payout",
            "amount": amount,
            "currency": "usd",
            "status": "failed",
            "failure_code": "account_closed",
            "failure_message": "The bank account has been closed"
        }}
    })
    .to_string()
}

/// payout.failed (webhook follow-up): records a payout_failures row + (via the notify cron)
/// exactly ONE payout_failed notification, idempotent on the payout id.
///
/// RED pre-fix: payout.failed fell into the deferred "acked but not acted on" arm — no
/// ledger row, no creator notification.
#[compio::test]
async fn payout_failed_records_failure_and_notifies_once() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "payout-failed").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    let acct = format!("acct_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.link_account(creator_id, &acct).await.expect("link");

    let po_id = format!("po_{}", Uuid::new_v4().simple());
    let evt = format!("evt_payout_{}", Uuid::new_v4().simple());
    let r = post_webhook!(app, payout_failed_body(&evt, &po_id, &acct, 7500), None);
    assert_eq!(r.status(), StatusCode::OK, "payout.failed processed");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "payout_failure_recorded");
    assert_eq!(ledger_count(&conn, &evt).await, 1, "event claimed");
    assert_eq!(payout_failure_count(&conn, creator_id).await, 1, "one failure row");

    // The notify cron emits exactly one payout_failed notification for this creator. We
    // assert off the DB send-ledger PER-CREATOR (parallel-safe per the PR-6 lesson: a
    // sibling's fleet-wide tick could deliver my creator's email into ITS recorder, but the
    // `billing_notifications` row is per-(creator,kind,transition) and immune).
    drive_notify_until_sent(&fx.state, &conn, creator_id, "payout_failed").await;
    assert_eq!(
        notification_sent_count(&conn, creator_id, "payout_failed").await,
        1,
        "exactly one payout_failed notification ledger row (sent) for this creator",
    );

    // Redelivery (different evt id, same po_…) is an idempotent no-op: no second row.
    let evt2 = format!("evt_payout2_{}", Uuid::new_v4().simple());
    let r2 = post_webhook!(app, payout_failed_body(&evt2, &po_id, &acct, 7500), None);
    assert_eq!(r2.status(), StatusCode::OK);
    let b2: Value = serde_json::from_slice(&test::read_body(r2).await).unwrap();
    assert_eq!(b2["status"], "duplicate", "same po_… is a no-op");
    assert_eq!(payout_failure_count(&conn, creator_id).await, 1, "still one failure row");
    let _ = zeroship_control::cron::billing_notify::tick(&fx.state).await;
    assert_eq!(
        notification_sent_count(&conn, creator_id, "payout_failed").await,
        1,
        "still exactly one payout_failed notification (no duplicate)",
    );
}

/// Count `sent` `billing_notifications` rows for a creator + kind (per-creator, immune to
/// the fleet-wide cron's sibling-recorder race — the PR-6 parallel-safe assertion).
async fn notification_sent_count(conn: &compio_postgres::Client, creator_id: Uuid, kind: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_notifications \
         WHERE creator_id = $1 AND kind = $2::text::zeroship.billing_notification_kind AND status = 'sent'",
        &[&creator_id, &kind],
    )
    .await
    .expect("count notifications")[0]
        .get::<_, i64>("n")
}

/// Drive the notify cron until MY creator's `(kind)` row is `sent` (or a bounded number of
/// ticks elapse). The cron's single-flight PG advisory lock means a given tick can LOSE to a
/// concurrent sibling test's sweep and win nothing — exactly the multi-node "loser skips"
/// path. So we retry rather than assume one tick delivers. Per-creator + DB-ledger-anchored,
/// so a sibling's tick delivering MY row (into ITS recorder) still flips MY ledger row to
/// `sent` and satisfies this loop (the PR-6 lesson).
async fn drive_notify_until_sent(
    state: &std::sync::Arc<AppState>,
    conn: &compio_postgres::Client,
    creator_id: Uuid,
    kind: &str,
) {
    for _ in 0..40 {
        // Drive the sweep DIRECTLY (not `tick`): under the default parallel runner many test
        // binaries compete for the cron's single advisory lock, so a `tick` loop can starve
        // (always losing the lock). The claim-before-send INSERT is the real multi-node
        // arbiter, so a direct sweep stays exactly-once-correct; it just guarantees the work
        // runs for THIS test's assertion.
        let _ = zeroship_control::cron::billing_notify::sweep(state).await;
        if notification_sent_count(conn, creator_id, kind).await >= 1 {
            return;
        }
    }
    panic!("notify cron did not deliver a `{kind}` notification for creator {creator_id} within 40 sweeps");
}

/// Count connect_checkout_failures rows for a creator.
async fn checkout_failure_count(conn: &compio_postgres::Client, creator_id: Uuid) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.connect_checkout_failures WHERE creator_id = $1",
        &[&creator_id],
    )
    .await
    .expect("count checkout failures")[0]
        .get::<_, i64>("n")
}

/// payment_intent.payment_failed body: a destination charge (transfer_data.destination)
/// PI that failed, carrying last_payment_error.
fn pi_failed_body(event_id: &str, pi_id: &str, account: &str, amount: i64) -> String {
    json!({
        "id": event_id,
        "type": "payment_intent.payment_failed",
        "created": 1_777_017_900i64,
        "data": { "object": {
            "id": pi_id,
            "object": "payment_intent",
            "amount": amount,
            "currency": "usd",
            "status": "requires_payment_method",
            "transfer_data": { "destination": account },
            "last_payment_error": { "code": "card_declined", "message": "Your card was declined." }
        }}
    })
    .to_string()
}

/// payment_intent.payment_failed (webhook follow-up): surfaces the failure as a
/// connect_checkout_failures row + (via the cron) exactly ONE checkout_failed
/// notification, idempotent on the PI id. No money moved — informational.
///
/// RED pre-fix: it fell into the deferred "acked but not acted on" arm — silently dropped.
#[compio::test]
async fn payment_intent_failed_surfaces_record_and_notifies_once() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "pi-failed").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    let acct = format!("acct_{}", Uuid::new_v4().simple());
    fx.state.stripe_store.link_account(creator_id, &acct).await.expect("link");

    let pi_id = format!("pi_{}", Uuid::new_v4().simple());
    let evt = format!("evt_pifail_{}", Uuid::new_v4().simple());
    let r = post_webhook!(app, pi_failed_body(&evt, &pi_id, &acct, 3200), None);
    assert_eq!(r.status(), StatusCode::OK, "payment_intent.payment_failed processed");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "checkout_failure_recorded");
    assert_eq!(ledger_count(&conn, &evt).await, 1, "event claimed");
    assert_eq!(checkout_failure_count(&conn, creator_id).await, 1, "one checkout-failure row");

    drive_notify_until_sent(&fx.state, &conn, creator_id, "checkout_failed").await;
    assert_eq!(
        notification_sent_count(&conn, creator_id, "checkout_failed").await,
        1,
        "exactly one checkout_failed notification ledger row (sent) for this creator",
    );

    // Redelivery (same pi_…) is a no-op.
    let evt2 = format!("evt_pifail2_{}", Uuid::new_v4().simple());
    let r2 = post_webhook!(app, pi_failed_body(&evt2, &pi_id, &acct, 3200), None);
    assert_eq!(r2.status(), StatusCode::OK);
    let b2: Value = serde_json::from_slice(&test::read_body(r2).await).unwrap();
    assert_eq!(b2["status"], "duplicate", "same pi_… is a no-op");
    assert_eq!(checkout_failure_count(&conn, creator_id).await, 1, "still one row");
}

// ════════════════════════════════════════════════════════════════════════════
// HTTP-boundary + signature + dispatch gaps (#9 / #2 / #7 / #12 / #29 / #27)
// ════════════════════════════════════════════════════════════════════════════
//
// #13 (event_processed Err → 500 fail-CLOSED) and #14 (mark_event_processed Err →
// log + ack-200 fail-OPEN) are COVERED-BY-REASONING, not by a faithful test —
// deliberately, because a faithful test is infeasible here WITHOUT an invasive
// production test-seam, which the brief forbids:
//
//   * #13 lives at `process_locked_event`: `event_processed()` Err → `err_json(500)`.
//     `event_processed` is a plain parameterised `SELECT 1 FROM stripe_events_seen
//     WHERE event_id = $1` on a clean, valid table. Forcing it to ERROR (not just
//     return a row) needs a DB-level fault (drop/rename/revoke the table, or kill the
//     connection mid-query). On this single shared test DB that would corrupt every
//     sibling test; control connects as a superuser so REVOKE doesn't bite; and there
//     is no natural row-state that makes a valid SELECT fail. The 500-fail-closed-
//     unclaimed CONTRACT is, however, already proven faithfully on a REAL error path
//     by `append_failure_leaves_event_unclaimed_not_silently_dropped` (a genuine
//     currency-CHECK violation → 500, event left unclaimed) and by the symmetric
//     `lock_event` Err → 500 arm immediately above it.
//
//   * #14 lives at the tail of `process_locked_event`: `mark_event_processed()` Err is
//     logged and the handler STILL acks 200 (do-NOT-5xx, since the idempotent handler
//     already applied its effect). `mark_event_processed` is an `INSERT … ON CONFLICT
//     (event_id) DO NOTHING` on the same two-column table — idempotent, so a duplicate
//     event_id is a no-op, NOT an error; and there is no other constraint a faithful
//     payload can violate. Forcing this INSERT to error likewise needs DB-level fault
//     injection / a prod seam. The deliberate fail-OPEN choice is documented inline at
//     the call site; exercising it faithfully is not feasible without a seam we won't add.

/// Compute Stripe's `v1` HMAC-SHA256 over `"{t}.{body}"` with `secret` — the SAME
/// construction `verify_stripe_signature` checks, so a test can build a header that
/// the REAL verifier accepts (not a hand-faked hex string).
fn stripe_v1(secret: &str, t: i64, body: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(format!("{t}.").as_bytes());
    mac.update(body.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// #9 (body cap): a body over MAX_WEBHOOK_BODY_BYTES (256 KiB) is rejected with 413
/// BEFORE any parse/HMAC/DB work. insecure_dev fixture (empty secret) so the body cap
/// — which runs ahead of the signature block — is the gate under test.
#[compio::test]
async fn webhook_oversized_body_rejected_413() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "boundary-413").await;
    let app = init_control!(fx);
    // 256 KiB + 1 byte of valid-ish JSON padding.
    let big = format!("{{\"id\":\"evt_big\",\"pad\":\"{}\"}}", "a".repeat(256 * 1024 + 1));
    let r = post_webhook!(app, big, None);
    assert_eq!(
        r.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "a webhook body over 256 KiB is rejected with 413 before parse/HMAC/DB"
    );
}

/// #9 (signature-header cap): a `stripe-signature` header over MAX_SIGNATURE_HEADER_BYTES
/// (4096) is rejected 400. Real secret + insecure_dev=false so the header-size guard
/// (inside the verify block) is reached.
#[compio::test]
async fn webhook_oversized_signature_header_rejected_400() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new_with_secret(&db_url, "boundary-sig", "whsec_test_boundary", false).await;
    let app = init_control!(fx);
    let body = json!({"id":"evt_x","type":"setup_intent.succeeded","created":1,"data":{"object":{"id":"seti_x"}}}).to_string();
    // A 4097-char header.
    let huge_sig = format!("t=1,{}", "v1=deadbeef,".repeat(400)); // > 4096 chars
    assert!(huge_sig.len() > 4096);
    let r = post_webhook!(app, &body, Some(huge_sig.as_str()));
    assert_eq!(
        r.status(),
        StatusCode::BAD_REQUEST,
        "a stripe-signature header over 4096 bytes is rejected with 400"
    );
}

/// #9 (missing signature header): with verification ON (real secret, insecure_dev=false),
/// a request carrying NO `stripe-signature` header is rejected 400 (the empty header has
/// no `t`/`v1` → verify fails). Proves the unsigned request never reaches a handler.
#[compio::test]
async fn webhook_missing_signature_header_rejected_400() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new_with_secret(&db_url, "boundary-nosig", "whsec_test_nosig", false).await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    let event_id = format!("evt_nosig_{}", Uuid::new_v4().simple());
    let body = setup_intent_body(&event_id, creator_id);
    // No stripe-signature header at all.
    let r = post_webhook!(app, &body, None);
    assert_eq!(
        r.status(),
        StatusCode::BAD_REQUEST,
        "a request with NO stripe-signature header is rejected 400 when verification is on"
    );
    assert_eq!(ledger_count(&conn, &event_id).await, 0, "unsigned event never claimed");
}

/// #2 (multi-v1 OR-fold): a signature header whose FIRST `v1=` is WRONG but a LATER `v1=`
/// MATCHES is ACCEPTED — pins the rotation-window OR-fold (the verifier must not early-exit
/// on the first mismatch). The event then dispatches and is claimed.
///
/// RED if `verify_stripe_signature` early-exits on the first non-matching v1.
#[compio::test]
async fn webhook_second_v1_matches_is_accepted() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let secret = "whsec_test_rotation";
    let fx = Fixture::new_with_secret(&db_url, "boundary-multiv1", secret, false).await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    let event_id = format!("evt_multiv1_{}", Uuid::new_v4().simple());
    let body = setup_intent_body(&event_id, creator_id);

    // `t` must be within tolerance of NOW (verify checks |now - t| <= 300).
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let good = stripe_v1(secret, t, &body);
    // FIRST v1 wrong, SECOND v1 correct — the rotation-window OR-fold must accept.
    let sig = format!("t={t},v1=00000000deadbeef,v1={good}");
    let r = post_webhook!(app, &body, Some(sig.as_str()));
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "a later matching v1 is accepted (rotation-window OR-fold; no early-exit on the first mismatch)"
    );
    assert_eq!(ledger_count(&conn, &event_id).await, 1, "the accepted event is claimed");
}

/// #7 (empty signing secret → verify Err): `verify_stripe_signature` with an EMPTY secret
/// returns Err — an empty HMAC key is a misconfiguration that must never silently accept.
/// Unit-level (the function is `pub`), no DB needed.
#[test]
fn verify_empty_secret_is_err() {
    let r = stripe_handlers::verify_stripe_signature(b"body", "t=1,v1=abc", "", 1, 300);
    assert!(r.is_err(), "an empty signing secret must error, never accept");
    assert!(
        r.unwrap_err().contains("empty"),
        "the error names the empty-secret misconfiguration"
    );
}

/// #7 (HTTP path, empty secret + insecure_dev=false → 500): the webhook endpoint with NO
/// configured signing secret and insecure_dev OFF rejects with 500 (misconfiguration —
/// fail closed, do not process). insecure_dev=true would be the dev bypass; this pins the
/// PROD posture.
#[compio::test]
async fn webhook_empty_secret_not_insecure_dev_is_500() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    // Empty secret, insecure_dev=false — the production-misconfig posture.
    let fx = Fixture::new_with_secret(&db_url, "boundary-emptysecret", "", false).await;
    let app = init_control!(fx);
    let body = json!({"id":"evt_emptysecret","type":"setup_intent.succeeded","created":1,"data":{"object":{"id":"seti_x"}}}).to_string();
    let r = post_webhook!(app, &body, Some("t=1,v1=abc"));
    assert_eq!(
        r.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "empty webhook secret with insecure_dev=false fails closed (500), never processes"
    );
}

/// #12 (concurrent dispatch e2e): two CONCURRENT `webhook()` calls for the SAME event_id
/// dispatch the handler EXACTLY ONCE — the per-event advisory lock serializes them, and
/// the second observes the first's claim and 200-acks as a `duplicate`. End-to-end
/// exactly-once (the lock PRIMITIVE is unit-tested in `lock_event_serializes_same_event`;
/// this pins the full webhook() path).
#[compio::test]
async fn concurrent_same_event_dispatches_once() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "concurrent-dispatch").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;
    let creator_id = make_user(&conn).await;
    let event_id = format!("evt_concur_{}", Uuid::new_v4().simple());
    let body = setup_intent_body(&event_id, creator_id);

    // Two concurrent deliveries of the SAME event.
    let req_a = test::TestRequest::post()
        .uri("/internal/webhooks/stripe")
        .header("content-type", "application/json")
        .set_payload(body.clone())
        .to_request();
    let req_b = test::TestRequest::post()
        .uri("/internal/webhooks/stripe")
        .header("content-type", "application/json")
        .set_payload(body.clone())
        .to_request();
    let (ra, rb) =
        futures::future::join(test::call_service(&app, req_a), test::call_service(&app, req_b)).await;

    // Both 200 (the serialized loser 200-acks the duplicate).
    assert_eq!(ra.status(), StatusCode::OK);
    assert_eq!(rb.status(), StatusCode::OK);
    let ba: Value = serde_json::from_slice(&test::read_body(ra).await).unwrap();
    let bb: Value = serde_json::from_slice(&test::read_body(rb).await).unwrap();
    let statuses = [ba["status"].as_str().unwrap_or(""), bb["status"].as_str().unwrap_or("")];
    assert!(
        statuses.contains(&"duplicate"),
        "exactly one of the concurrent deliveries 200-acks as a duplicate (got {statuses:?})"
    );
    // The handler ran EXACTLY ONCE (one audit row) and the event is claimed once.
    assert_eq!(
        setup_audit_count(&conn, creator_id, &event_id).await,
        1,
        "the handler dispatched exactly once across the concurrent deliveries"
    );
    assert_eq!(ledger_count(&conn, &event_id).await, 1, "event claimed exactly once");
}

/// #29 (payout.failed with NO connected account): a `payout.failed` carrying neither a
/// top-level `account` nor a destination → benign 200 ack (`no_connected_account`), no
/// payout_failures row written.
#[compio::test]
async fn payout_failed_without_connected_account_acks_no_row() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "payout-noacct").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;

    let po_id = format!("po_{}", Uuid::new_v4().simple());
    let evt = format!("evt_payout_noacct_{}", Uuid::new_v4().simple());
    // payout.failed body with NO top-level `account` and NO destination.
    let body = json!({
        "id": evt,
        "type": "payout.failed",
        "created": 1_777_017_800i64,
        "data": { "object": {
            "id": po_id,
            "object": "payout",
            "amount": 5000,
            "currency": "usd",
            "status": "failed"
        }}
    })
    .to_string();
    let r = post_webhook!(app, &body, None);
    assert_eq!(r.status(), StatusCode::OK, "benign ack — not attributable");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "no_connected_account");
    // Scope to THIS test's payout id: a global COUNT(*) races concurrent siblings in the
    // same (multi-threaded) binary that legitimately write payout_failures rows.
    let n = conn
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.payout_failures WHERE provider_payout_id = $1",
            &[&po_id],
        )
        .await
        .expect("count")[0]
        .get::<_, i64>("n");
    assert_eq!(n, 0, "no payout_failures row written for this payout");
}

/// #29 (payment_intent.payment_failed with NO connected account): a platform (non-Connect)
/// PI failure → benign 200 ack (`no_connected_account`), no connect_checkout_failures row.
#[compio::test]
async fn payment_intent_failed_without_connected_account_acks_no_row() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "pifail-noacct").await;
    let app = init_control!(fx);
    let conn = side_conn(&db_url).await;

    let pi_id = format!("pi_{}", Uuid::new_v4().simple());
    let evt = format!("evt_pifail_noacct_{}", Uuid::new_v4().simple());
    // No transfer_data.destination, no on_behalf_of, no top-level account.
    let body = json!({
        "id": evt,
        "type": "payment_intent.payment_failed",
        "created": 1_777_017_900i64,
        "data": { "object": {
            "id": pi_id,
            "object": "payment_intent",
            "amount": 3200,
            "currency": "usd",
            "status": "requires_payment_method",
            "last_payment_error": { "code": "card_declined", "message": "declined" }
        }}
    })
    .to_string();
    let r = post_webhook!(app, &body, None);
    assert_eq!(r.status(), StatusCode::OK, "benign ack — not a Connect checkout");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "no_connected_account");
    // Scope to THIS test's pi id: a global COUNT(*) races concurrent siblings in the same
    // (multi-threaded) binary that legitimately write connect_checkout_failures rows.
    let n = conn
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.connect_checkout_failures WHERE provider_payment_intent_id = $1",
            &[&pi_id],
        )
        .await
        .expect("count")[0]
        .get::<_, i64>("n");
    assert_eq!(n, 0, "no connect_checkout_failures row written for this pi");
}

/// #27 (account.updated for an UNLINKED account): an `account.updated` for an `acct_…` we
/// never linked updates 0 rows → `account_not_linked` ack, no error. The event is still
/// claimed (it was validly delivered; re-processing it would be a no-op).
#[compio::test]
async fn account_updated_unlinked_account_acks_not_linked() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "acct-unlinked").await;
    let app = init_control!(fx);

    // An acct_ we NEVER linked to any creator (valid acct_ format, just no link row).
    let acct = format!("acct_{}", Uuid::new_v4().simple());
    let evt = format!("evt_acct_unlinked_{}", Uuid::new_v4().simple());
    let body = json!({
        "id": evt,
        "type": "account.updated",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": acct,
            "object": "account",
            "charges_enabled": false,
            "payouts_enabled": false,
            "details_submitted": true
        }}
    })
    .to_string();
    let r = post_webhook!(app, &body, None);
    assert_eq!(r.status(), StatusCode::OK, "account.updated for an unlinked account acks 200 (no error)");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(
        b["status"], "account_not_linked",
        "an account.updated for an acct_ we never linked is a benign no-op (0-row flag update)"
    );
}
