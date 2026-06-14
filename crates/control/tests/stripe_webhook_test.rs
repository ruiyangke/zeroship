//! Live-PG regression tests for Stripe webhook audit coverage.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    stripe_handlers, token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
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
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".to_string(),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            insecure_dev,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::new(control_pg_client),
            hydra_admin_url: "http://127.0.0.1:4445".to_string(),
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
            pairwise_salt: [0u8; 32],
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

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(web::App::new().state($fx.state.clone()).service(
            web::resource("/internal/webhooks/stripe").route(web::post().to(stripe_handlers::webhook)),
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
    // An `invoice.paid` carrying `metadata.creator_id` also flows through the
    // Stream-2 Connect payout-ledger path (record_payout), which FKs to a linked
    // Connect account. Link one so that path succeeds and the webhook returns 200 —
    // the infra `invoice_payments` append (PR-1) is what this test asserts.
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
