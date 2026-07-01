//! PR-8 regression tests for billing-ops gap #26: disputes / chargebacks (the FINAL PR).
//!
//! FAITHFUL by construction (gap #26 PR-8 review, CRITICAL-2): a REAL Stripe Dispute object
//! carries NO `invoice` field — only `charge` (`ch_…`) and `payment_intent` (`pi_…`). The
//! pre-fix suite masked the dead resolution path by putting an `in_…` in the dispute's
//! `charge`; that NEVER appears at Stripe and let the suite pass while production was dead.
//! Here every test FIRST drives the REAL `invoice.paid` webhook (carrying the settling
//! `pi_…`) so the handler records the `pi_…`→invoice `billing_provider_refs` linkage EXACTLY
//! as production does, and the dispute object then names that REAL `pi_…`. No `in_…` ever
//! appears on a dispute object. The whole chain runs end to end against a live, migrated
//! Postgres: `stripe_handlers::webhook` (signature path, JSON parse, dispatch) →
//! `record_infra_payment` (charge row + pi_/ch_ linkage) → `charge.dispute.*` →
//! `disputes::record_dispute_*` → `invoice_payments::append_dispute_row`. The over-refund
//! interaction runs the REAL `refund::issue_refund` against the REAL trigger. Gated on
//! `CONTROL_TEST_DB`; silent skip otherwise.
//!
//! These FAIL against the broken resolution (an `in_…`-on-dispute test would resolve
//! nothing → no dispute recorded → assertions on the debit / cap / reversal all fail):
//!   (a) `charge.dispute.created` (naming the REAL `pi_…`) records a `billing_disputes` row
//!       + a `dispute_debit` lowering cash_collected; a redelivered event (same du_…) does
//!       NOT double-debit.
//!   (b) after a dispute_debit, a cash refund that WOULD have fit the pre-dispute cap is
//!       now rejected by the over-refund trigger (the cap auto-tightened).
//!   (c) `.closed won` appends a `dispute_reversal` restoring cash_collected; `.closed
//!       lost` leaves the debit.
//!   (d) the dispute produces exactly ONE `disputed` notification (asserted off the DB
//!       ledger, per-creator, via the REAL notify cron tick).
//!   (e) a dispute on a credited/refunded invoice doesn't corrupt credit/refund balances.
//!   (f) the PR-8 schema objects exist on the migrated DB.
//!   (g) LIFECYCLE (CRITICAL-3): a won→(late/replayed)lost reorder is rejected — the row
//!       stays `won` and cash stays restored (no over-refund window).
//!   (h) ORDER-INDEPENDENCE (MAJOR-4): `.closed won` BEFORE `.created` ends with the dispute
//!       won, debit + reversal both present (net cash restored), and the late `.created`
//!       does not resurrect it to `open`.
//!
//! Parallel-safe: every test seeds its OWN creator (unique email) + its own invoice +
//! globally-unique Stripe ids (`du_…`/`evt_…`/`in_…`/`pi_…` carry a fresh UUID), and every
//! assertion is scoped to that creator/invoice — so the DEFAULT parallel cargo runner
//! (and the shared notify cron sweep) never causes cross-test interference.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::notify::{BillingNotificationKind, RecordingNotifier};
use zeroship_control::refund::{issue_refund, NativeRefundProvider, RefundDestination, RefundOutcome};
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
    let path = std::env::temp_dir().join(format!("zs-dispute-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

/// Fixture wiring the webhook handler with a `RecordingNotifier` so test (d) can observe
/// the `disputed` send the cron drives. Returns the state + the notifier handle.
struct Fixture {
    state: Arc<AppState>,
    notifier: Arc<RecordingNotifier>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Fixture {
    async fn new(db_url: &str, label: &str) -> Self {
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

        let notifier = Arc::new(RecordingNotifier::new());
        let state = Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            blob_store,
            control_key: SecretString::new("test-control-key".to_string()),
            master_key: SecretString::new("test-master-key".to_string()),
            // insecure_dev with empty secret ⇒ signature verification skipped.
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".to_string(),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            insecure_dev: true,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::new(control_pg_client),
            hydra_admin_url: "http://127.0.0.1:4445".to_string(),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(zeroship_authn::PatIssuer::dev_insecure()),
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
            notifier: notifier.clone(),
            pairwise_salt: [0u8; 32],
            projected_charge_cache: std::sync::Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        });

        Self { state, notifier, blob_root, deploy_tmp_dir }
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

macro_rules! post_webhook {
    ($app:expr, $body:expr) => {{
        let req = test::TestRequest::post()
            .uri("/internal/webhooks/stripe")
            .header("content-type", "application/json")
            .set_payload($body.to_string())
            .to_request();
        test::call_service(&$app, req).await
    }};
}

// ─── seeding helpers (parallel-safe: unique creator/invoice/refs per call) ───

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

async fn make_creator(conn: &compio_postgres::Client) -> Uuid {
    let creator: Uuid = conn
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, 'dispute-test') RETURNING id",
            &[&format!("dsp-{}@test.invalid", Uuid::new_v4().simple())],
        )
        .await
        .expect("insert user")[0]
        .get("id");
    conn.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) ON CONFLICT DO NOTHING",
        &[&creator],
    )
    .await
    .expect("creator_billing");
    // A `cus_…` ↔ creator mapping so the REAL infra `invoice.paid` path resolves the
    // creator via CUSTOMER reverse-resolve (the genuine infra-invoice shape: Stripe's
    // auto-generated subscription invoices carry no creator metadata). This keeps the seed
    // off the Stream-2 Connect payout fall-through entirely.
    let cus = format!("cus_dsp_{}", Uuid::new_v4().simple());
    conn.execute(
        "INSERT INTO zeroship.billing_customer_refs (creator_id, provider, external_id) \
         VALUES ($1, 'stripe', $2) ON CONFLICT DO NOTHING",
        &[&creator, &cus],
    )
    .await
    .expect("customer ref");
    creator
}

/// Resolve the `cus_…` mapped to a creator (seeded in `make_creator`) — the infra
/// `invoice.paid` body names it so the handler reverse-resolves the creator.
async fn creator_customer(conn: &compio_postgres::Client, creator: Uuid) -> String {
    conn.query(
        "SELECT external_id FROM zeroship.billing_customer_refs \
         WHERE creator_id = $1 AND provider = 'stripe'",
        &[&creator],
    )
    .await
    .expect("customer ref")[0]
        .get::<_, String>("external_id")
}

fn this_period() -> chrono::NaiveDate {
    period_offset(0)
}

/// A first-of-month period `months` before this month — lets one creator hold several
/// distinct-period invoices without colliding on the `invoices_active_period_claim`
/// partial unique index.
fn period_offset(months: i64) -> chrono::NaiveDate {
    use chrono::Datelike;
    let now = chrono::Utc::now().date_naive();
    let total = i64::from(now.year()) * 12 + i64::from(now.month0()) - months;
    let year = (total.div_euclid(12)) as i32;
    let month0 = total.rem_euclid(12) as u32;
    chrono::NaiveDate::from_ymd_opt(year, month0 + 1, 1).unwrap()
}

/// FAITHFUL seed (CRITICAL-2): seed a FINALIZED infra invoice + its `ref_kind='invoice'`
/// linkage, then drive the REAL `invoice.paid` webhook (via `$app`) so the production
/// handler (`record_infra_payment`) records BOTH the `charge` `invoice_payments` row AND
/// the settling `payment_intent` (`pi_…`)→invoice `billing_provider_refs` linkage that
/// dispute resolution depends on. Yields `(internal_invoice_id, pi_id)` — the `pi_…` is what
/// the dispute object then names (NEVER an `in_…`, which a Stripe dispute never carries). A
/// per-creator-unique `period` avoids the partial-unique-index collision.
///
/// A macro (not a fn) so it can drive the REAL webhook through `$app` without naming ntex's
/// opaque `init_service` Service type.
macro_rules! seed_paid_invoice_period {
    ($app:expr, $conn:expr, $creator:expr, $total:expr, $period:expr) => {{
        let inv = zeroship_core::typed_id::new_invoice_id();
        $conn
            .execute(
                "INSERT INTO zeroship.invoices \
                   (id, creator_id, period, status, subtotal_cents, credit_cents, tax_cents, \
                    total_cents, finalized_at) \
                 VALUES ($1, $2, $3::date, 'finalized', $4, 0, 0, $4, NOW())",
                &[&inv, &$creator, &$period, &($total as i64)],
            )
            .await
            .expect("finalized invoice");
        let provider_invoice = format!("in_dsp_{}", Uuid::new_v4().simple());
        $conn
            .execute(
                "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
                 VALUES ($1, 'stripe', 'invoice', $2)",
                &[&inv, &provider_invoice],
            )
            .await
            .expect("provider ref");

        // Drive the REAL invoice.paid webhook — the genuine infra shape: invoice_kind=infra
        // marker + a `customer` (cus_…) the handler reverse-resolves to the creator (NO
        // creator metadata, so no Stream-2 payout fall-through), naming the settling
        // payment_intent (pi_…) which the handler persists as the resolution linkage.
        let cus = creator_customer(&$conn, $creator).await;
        let pi = format!("pi_dsp_{}", Uuid::new_v4().simple());
        let paid_body = json!({
            "id": format!("evt_paid_{}", Uuid::new_v4().simple()),
            "type": "invoice.paid",
            "created": 1_777_000_000i64,
            "data": { "object": {
                "id": provider_invoice,
                "amount_paid": ($total as i64),
                "currency": "usd",
                "payment_intent": pi,
                "customer": cus,
                "metadata": { "invoice_kind": "infra" }
            }}
        });
        let resp = post_webhook!($app, paid_body);
        assert_eq!(resp.status(), StatusCode::OK, "invoice.paid seed webhook must 200");
        assert_eq!(
            cash_collected(&$conn, &inv).await,
            $total as i64,
            "invoice.paid recorded the cash via the REAL handler"
        );
        assert_eq!(
            provider_ref_count(&$conn, &pi, "payment_intent").await,
            1,
            "invoice.paid recorded the pi_…→invoice linkage (the dispute-resolution anchor)"
        );
        (inv, pi)
    }};
}

/// The common case: a paid infra invoice for THIS month.
macro_rules! seed_paid_invoice {
    ($app:expr, $conn:expr, $creator:expr, $total:expr) => {
        seed_paid_invoice_period!($app, $conn, $creator, $total, this_period())
    };
}

/// Count `billing_provider_refs` rows for a given external id + ref_kind (the dispute
/// resolution linkage assertion).
async fn provider_ref_count(conn: &compio_postgres::Client, external_id: &str, ref_kind: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_provider_refs \
         WHERE provider = 'stripe' AND ref_kind = $2 AND external_id = $1",
        &[&external_id, &ref_kind],
    )
    .await
    .expect("provider ref count")[0]
        .get::<_, i64>("n")
}

/// A `charge.dispute.created` carrying a REAL settling `pi_…` (a Stripe Dispute object has
/// NO `invoice` field — only `payment_intent`/`charge`).
fn dispute_created_body(evt: &str, du: &str, payment_intent: &str, amount: i64) -> String {
    json!({
        "id": evt,
        "type": "charge.dispute.created",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": du,
            "amount": amount,
            "currency": "usd",
            "status": "needs_response",
            "reason": "fraudulent",
            "payment_intent": payment_intent,
            "evidence_details": { "due_by": 1_779_000_000i64 }
        }}
    })
    .to_string()
}

/// A `charge.dispute.closed` carrying the REAL settling `pi_…` so a close-before-create
/// (MAJOR-4) can resolve the anchor invoice. `amount` lets a close-first seed a terminal row.
fn dispute_closed_body(evt: &str, du: &str, status: &str, payment_intent: &str, amount: i64) -> String {
    json!({
        "id": evt,
        "type": "charge.dispute.closed",
        "created": 1_777_900_000i64,
        "data": { "object": {
            "id": du,
            "status": status,
            "currency": "usd",
            "amount": amount,
            "payment_intent": payment_intent
        }}
    })
    .to_string()
}

async fn cash_collected(conn: &compio_postgres::Client, inv: &str) -> i64 {
    zeroship_control::invoice_payments::cash_collected(conn, inv)
        .await
        .expect("cash_collected")
}

async fn dispute_status(conn: &compio_postgres::Client, du: &str) -> Option<String> {
    let rows = conn
        .query(
            "SELECT status::text AS s FROM zeroship.billing_disputes WHERE provider_dispute_id = $1",
            &[&du],
        )
        .await
        .expect("dispute status");
    rows.first().map(|r| r.get::<_, String>("s"))
}

async fn dispute_row_count(conn: &compio_postgres::Client, du: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_disputes WHERE provider_dispute_id = $1",
        &[&du],
    )
    .await
    .expect("count disputes")[0]
        .get::<_, i64>("n")
}

async fn payment_kind_count(conn: &compio_postgres::Client, inv: &str, kind: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.invoice_payments \
         WHERE invoice_id = $1 AND kind = $2::text::zeroship.invoice_payment_kind",
        &[&inv, &kind],
    )
    .await
    .expect("count payments")[0]
        .get::<_, i64>("n")
}

// ───────────────────────────────────────────────────────────────────────────
// (a) created records the dispute + a dispute_debit; redelivery does NOT double-debit.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_created_records_debit_and_is_idempotent() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "created-idem").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    let (inv, pi) = seed_paid_invoice!(app, conn, creator, 6000);

    assert_eq!(cash_collected(&conn, &inv).await, 6000, "cash starts at the charge");

    let du = format!("du_{}", Uuid::new_v4().simple());
    let evt1 = format!("evt_dc1_{}", Uuid::new_v4().simple());
    let r1 = post_webhook!(app, dispute_created_body(&evt1, &du, &pi, 6000));
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value = serde_json::from_slice(&test::read_body(r1).await).unwrap();
    assert_eq!(b1["status"], "dispute_recorded");

    // One dispute row (open) + one dispute_debit row; cash_collected dropped to 0.
    assert_eq!(dispute_row_count(&conn, &du).await, 1, "one dispute row");
    assert_eq!(dispute_status(&conn, &du).await.as_deref(), Some("open"));
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1);
    assert_eq!(cash_collected(&conn, &inv).await, 0, "dispute_debit clawed back the cash");

    // Redelivery under a DIFFERENT event id (so stripe_events_seen does NOT dedup it) —
    // the du_… dedup must still prevent a second debit.
    let evt2 = format!("evt_dc2_{}", Uuid::new_v4().simple());
    let r2 = post_webhook!(app, dispute_created_body(&evt2, &du, &pi, 6000));
    assert_eq!(r2.status(), StatusCode::OK);

    assert_eq!(dispute_row_count(&conn, &du).await, 1, "still exactly one dispute row");
    assert_eq!(
        payment_kind_count(&conn, &inv, "dispute_debit").await,
        1,
        "redelivery must NOT add a second dispute_debit (idempotent on du_…)"
    );
    assert_eq!(cash_collected(&conn, &inv).await, 0, "cash_collected un-doubled");

    // The reason/evidence_due_at were captured.
    let row = conn
        .query(
            "SELECT reason, evidence_due_at IS NOT NULL AS has_due FROM zeroship.billing_disputes \
             WHERE provider_dispute_id = $1",
            &[&du],
        )
        .await
        .expect("read dispute")[0]
        .clone();
    assert_eq!(row.get::<_, Option<String>>("reason").as_deref(), Some("fraudulent"));
    assert!(row.get::<_, bool>("has_due"), "evidence_due_at captured from due_by");
}

// ───────────────────────────────────────────────────────────────────────────
// (b) after a dispute_debit, a cash refund that fit the pre-dispute cap is rejected.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_debit_tightens_over_refund_cap() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "cap-tighten").await;
    let app = init_control!(fx);
    let mut conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    let (inv, pi) = seed_paid_invoice!(app, conn, creator, 6000);

    // PRE-dispute: a $50 cash refund fits the $60 cash cap. (Prove the baseline fits by
    // checking cash_collected, then NOT issuing — we want the dispute to flip it.)
    assert_eq!(cash_collected(&conn, &inv).await, 6000);

    // Dispute claws back $40 → cash_collected = $20.
    let du = format!("du_{}", Uuid::new_v4().simple());
    let evt = format!("evt_cap_{}", Uuid::new_v4().simple());
    let r = post_webhook!(app, dispute_created_body(&evt, &du, &pi, 4000));
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(cash_collected(&conn, &inv).await, 2000, "cap auto-tightened to $20");

    // A $50 cash refund WOULD have fit the pre-dispute $60 cap, but now exceeds the
    // tightened $20 — the REAL over-refund trigger (read through issue_refund) rejects it.
    let provider = NativeRefundProvider;
    let outcome = issue_refund(
        &mut conn,
        &provider,
        &inv,
        5000,
        5000,
        0,
        RefundDestination::Cash,
        Some("post-dispute over-refund"),
        &format!("idem-cap-{}", Uuid::new_v4().simple()),
    )
    .await
    .expect("issue_refund call");
    assert!(
        matches!(outcome, RefundOutcome::OverRefund(_)),
        "a $50 cash refund must be rejected by the dispute-tightened cap, got {outcome:?}"
    );

    // A refund WITHIN the tightened cap ($20) still succeeds — proving it's the dispute,
    // not a blanket block.
    let ok = issue_refund(
        &mut conn,
        &provider,
        &inv,
        2000,
        2000,
        0,
        RefundDestination::Cash,
        Some("within tightened cap"),
        &format!("idem-cap-ok-{}", Uuid::new_v4().simple()),
    )
    .await
    .expect("issue_refund call");
    assert!(matches!(ok, RefundOutcome::Issued { .. }), "a $20 refund still fits, got {ok:?}");
}

// ───────────────────────────────────────────────────────────────────────────
// (c) .closed won restores cash via dispute_reversal; .closed lost leaves the debit.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_closed_won_restores_cash_lost_leaves_debit() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "closed").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;

    // --- WON path --- (distinct periods so both invoices fit the partial unique index)
    let (inv_won, pi_won) = seed_paid_invoice_period!(app, conn, creator, 6000, period_offset(0));
    let du_won = format!("du_won_{}", Uuid::new_v4().simple());
    let r = post_webhook!(
        app,
        dispute_created_body(&format!("evt_w1_{}", Uuid::new_v4().simple()), &du_won, &pi_won, 6000)
    );
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(cash_collected(&conn, &inv_won).await, 0, "debited to 0 on created");

    let rc = post_webhook!(app, dispute_closed_body(&format!("evt_w2_{}", Uuid::new_v4().simple()), &du_won, "won", &pi_won, 6000));
    assert_eq!(rc.status(), StatusCode::OK);
    let bc: Value = serde_json::from_slice(&test::read_body(rc).await).unwrap();
    assert_eq!(bc["dispute_status"], "won");
    assert_eq!(dispute_status(&conn, &du_won).await.as_deref(), Some("won"));
    assert_eq!(payment_kind_count(&conn, &inv_won, "dispute_reversal").await, 1);
    assert_eq!(cash_collected(&conn, &inv_won).await, 6000, "won restores the budget");

    // Redelivered won close must NOT double-restore.
    let rc2 = post_webhook!(app, dispute_closed_body(&format!("evt_w3_{}", Uuid::new_v4().simple()), &du_won, "won", &pi_won, 6000));
    assert_eq!(rc2.status(), StatusCode::OK);
    assert_eq!(payment_kind_count(&conn, &inv_won, "dispute_reversal").await, 1, "no double reversal");
    assert_eq!(cash_collected(&conn, &inv_won).await, 6000);

    // --- LOST path ---
    let (inv_lost, pi_lost) = seed_paid_invoice_period!(app, conn, creator, 5000, period_offset(1));
    let du_lost = format!("du_lost_{}", Uuid::new_v4().simple());
    post_webhook!(
        app,
        dispute_created_body(&format!("evt_l1_{}", Uuid::new_v4().simple()), &du_lost, &pi_lost, 5000)
    );
    assert_eq!(cash_collected(&conn, &inv_lost).await, 0, "debited on created");
    let rl = post_webhook!(app, dispute_closed_body(&format!("evt_l2_{}", Uuid::new_v4().simple()), &du_lost, "lost", &pi_lost, 5000));
    assert_eq!(rl.status(), StatusCode::OK);
    assert_eq!(dispute_status(&conn, &du_lost).await.as_deref(), Some("lost"));
    assert_eq!(payment_kind_count(&conn, &inv_lost, "dispute_reversal").await, 0, "lost adds NO reversal");
    assert_eq!(cash_collected(&conn, &inv_lost).await, 0, "lost leaves the debit standing");
}

// ───────────────────────────────────────────────────────────────────────────
// (d) the dispute produces exactly ONE `disputed` notification (per-creator ledger).
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_produces_exactly_one_disputed_notification() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "notify").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    let (_inv, pi) = seed_paid_invoice!(app, conn, creator, 6000);

    let du = format!("du_{}", Uuid::new_v4().simple());
    let r = post_webhook!(
        app,
        dispute_created_body(&format!("evt_n1_{}", Uuid::new_v4().simple()), &du, &pi, 6000)
    );
    assert_eq!(r.status(), StatusCode::OK);

    // Resolve our dsp_… id for the per-creator ledger / key-prefix assertions.
    let dsp_id: String = conn
        .query(
            "SELECT id FROM zeroship.billing_disputes WHERE provider_dispute_id = $1",
            &[&du],
        )
        .await
        .expect("dsp id")[0]
        .get("id");

    // Drive the REAL notify cron tick (it sweeps the shared DB; we scope by creator).
    let key_prefix = format!("{creator}:disputed:");
    let _ = zeroship_control::cron::billing_notify::tick(&fx.state)
        .await
        .expect("notify tick 1");

    // Exactly one `disputed` ledger row for THIS creator, marked sent, keyed on the dsp_…
    let ledger = conn
        .query(
            "SELECT status::text AS s, transition_id FROM zeroship.billing_notifications \
             WHERE creator_id = $1 AND kind = 'disputed'",
            &[&creator],
        )
        .await
        .expect("ledger");
    assert_eq!(ledger.len(), 1, "exactly one disputed ledger row for this creator");
    assert_eq!(ledger[0].get::<_, String>("transition_id"), dsp_id, "keyed on the dsp_… id");
    assert_eq!(ledger[0].get::<_, String>("s"), "sent");

    // Exactly one delivery for THIS creator's disputed key (scoped, parallel-safe).
    assert_eq!(
        fx.notifier.delivered_for_key_prefix(&key_prefix),
        1,
        "exactly one disputed email delivered for this creator"
    );

    // A second cron tick must NOT re-send (claim ledger already `sent`).
    let _ = zeroship_control::cron::billing_notify::tick(&fx.state)
        .await
        .expect("notify tick 2");
    assert_eq!(
        fx.notifier.delivered_for_key_prefix(&key_prefix),
        1,
        "the disputed notification is send-once across cron ticks"
    );
    assert!(
        fx.notifier.delivered_for_kind(BillingNotificationKind::Disputed) >= 1,
        "the disputed kind was exercised"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// (e) a dispute on a credited/refunded invoice doesn't corrupt credit/refund balances.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_on_credited_refunded_invoice_preserves_balances() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "credited").await;
    let app = init_control!(fx);
    let mut conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;

    // A partially-credit-covered invoice: subtotal 8000, credit 2000, total 6000, cash 6000.
    // Seed the invoice + its 'invoice' ref, then drive the REAL invoice.paid (naming pi_…)
    // so the charge row + pi_…→invoice linkage land via the production handler.
    let inv = zeroship_core::typed_id::new_invoice_id();
    conn.execute(
        "INSERT INTO zeroship.invoices \
           (id, creator_id, period, status, subtotal_cents, credit_cents, tax_cents, total_cents, finalized_at) \
         VALUES ($1, $2, $3::date, 'finalized', 8000, 2000, 0, 6000, NOW())",
        &[&inv, &creator, &this_period()],
    )
    .await
    .expect("finalized credited invoice");
    let provider_invoice = format!("in_dsp_{}", Uuid::new_v4().simple());
    conn.execute(
        "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'invoice', $2)",
        &[&inv, &provider_invoice],
    )
    .await
    .expect("provider ref");
    let cus = creator_customer(&conn, creator).await;
    let pi = format!("pi_dsp_{}", Uuid::new_v4().simple());
    let paid_body = json!({
        "id": format!("evt_paid_{}", Uuid::new_v4().simple()),
        "type": "invoice.paid",
        "created": 1_777_000_000i64,
        "data": { "object": {
            "id": provider_invoice,
            "amount_paid": 6000,
            "currency": "usd",
            "payment_intent": pi,
            "customer": cus,
            "metadata": { "invoice_kind": "infra" }
        }}
    });
    let pr = post_webhook!(app, paid_body);
    assert_eq!(pr.status(), StatusCode::OK, "invoice.paid seed");
    assert_eq!(cash_collected(&conn, &inv).await, 6000, "cash via real handler");

    // Record a $2000 credit-destination refund first (a goodwill credit-back). This appends
    // a refund_to_credit grant (credit balance +2000) — independent of cash.
    let provider = NativeRefundProvider;
    let credit_refund = issue_refund(
        &mut conn,
        &provider,
        &inv,
        2000,
        2000,
        0,
        RefundDestination::Credit,
        Some("goodwill"),
        &format!("idem-credit-{}", Uuid::new_v4().simple()),
    )
    .await
    .expect("credit refund");
    assert!(matches!(credit_refund, RefundOutcome::Issued { .. }));

    // Snapshot credit balance + refund count BEFORE the dispute.
    let credit_before = credit_balance(&conn, creator).await;
    let refunds_before = refund_count(&conn, &inv).await;
    assert_eq!(credit_before, 2000, "the refund_to_credit grant is on the balance");
    assert_eq!(refunds_before, 1);

    // Now a dispute claws back $6000. It is PURELY a cash-collected adjustment (a negative
    // invoice_payments row) — it must NOT touch credit_ledger or refunds.
    let du = format!("du_{}", Uuid::new_v4().simple());
    let r = post_webhook!(
        app,
        dispute_created_body(&format!("evt_e1_{}", Uuid::new_v4().simple()), &du, &pi, 6000)
    );
    assert_eq!(r.status(), StatusCode::OK);

    // Cash dropped, but credit balance + refund rows are UNCHANGED (no double-count / corruption).
    assert_eq!(cash_collected(&conn, &inv).await, 0, "dispute_debit lowered cash to 0");
    assert_eq!(
        credit_balance(&conn, creator).await,
        credit_before,
        "the dispute did NOT touch the credit ledger"
    );
    assert_eq!(
        refund_count(&conn, &inv).await,
        refunds_before,
        "the dispute did NOT add/alter a refund row"
    );
    // And the dispute_debit is the SOLE new payment movement.
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1);
}

async fn credit_balance(conn: &compio_postgres::Client, creator: Uuid) -> i64 {
    conn.query(
        "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS b FROM zeroship.credit_ledger WHERE creator_id = $1",
        &[&creator],
    )
    .await
    .expect("credit balance")[0]
        .get::<_, i64>("b")
}

async fn refund_count(conn: &compio_postgres::Client, inv: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.refunds WHERE invoice_id = $1",
        &[&inv],
    )
    .await
    .expect("refund count")[0]
        .get::<_, i64>("n")
}

// ───────────────────────────────────────────────────────────────────────────
// (g) LIFECYCLE (CRITICAL-3): a won → (late/replayed) lost reorder is REJECTED — the row
//     stays `won`, the reversal stands, and the over-refund cap reflects the terminal
//     (won) outcome (no stranded restored cash on a lost dispute). Pre-fix the close
//     UPDATE was unconditional, so the late `lost` flipped status to lost while the
//     reversal (restored cash) remained — an over-refund window.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_won_then_late_lost_is_rejected_cash_stays_restored() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "won-then-lost").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    let (inv, pi) = seed_paid_invoice!(app, conn, creator, 6000);

    let du = format!("du_wl_{}", Uuid::new_v4().simple());
    // created → open, debit to 0.
    let r = post_webhook!(app, dispute_created_body(&format!("evt_wl1_{}", Uuid::new_v4().simple()), &du, &pi, 6000));
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(cash_collected(&conn, &inv).await, 0);

    // closed WON → reversal restores cash to 6000; status won.
    let rw = post_webhook!(app, dispute_closed_body(&format!("evt_wl2_{}", Uuid::new_v4().simple()), &du, "won", &pi, 6000));
    assert_eq!(rw.status(), StatusCode::OK);
    assert_eq!(dispute_status(&conn, &du).await.as_deref(), Some("won"));
    assert_eq!(cash_collected(&conn, &inv).await, 6000, "won restored the cash");

    // A LATE / replayed closed LOST must NOT flip the terminal won dispute. The handler
    // still 200-acks (a no-op), but the row stays won and the cash stays restored — no
    // over-refund window opens on a now-falsely-lost dispute.
    let rl = post_webhook!(app, dispute_closed_body(&format!("evt_wl3_{}", Uuid::new_v4().simple()), &du, "lost", &pi, 6000));
    assert_eq!(rl.status(), StatusCode::OK, "the late lost is acked, not 500");
    assert_eq!(
        dispute_status(&conn, &du).await.as_deref(),
        Some("won"),
        "the won→lost reorder is rejected; status stays won"
    );
    assert_eq!(
        cash_collected(&conn, &inv).await,
        6000,
        "the restored cash reflects the terminal WON outcome (no stranded over-refund window)"
    );
    // Exactly one reversal, zero extra debits from the late lost.
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_reversal").await, 1);
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1);
}

// ───────────────────────────────────────────────────────────────────────────
// (h) ORDER-INDEPENDENCE (MAJOR-4): `.closed won` delivered BEFORE `.created` (legal
//     at-least-once reordering) ends with the dispute won, debit + reversal both present
//     (net cash restored), and the late `.created` does NOT resurrect it to `open`. Pre-fix
//     the close-before-create was dropped (no row) and the later created left it open
//     forever.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_closed_won_before_created_is_order_independent() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "close-first").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    let (inv, pi) = seed_paid_invoice!(app, conn, creator, 6000);
    assert_eq!(cash_collected(&conn, &inv).await, 6000);

    let du = format!("du_cf_{}", Uuid::new_v4().simple());

    // closed WON arrives FIRST (no created yet). It must seed a terminal won row applying
    // debit + reversal (net cash unchanged = 6000), resolving the invoice via the pi_….
    let rc = post_webhook!(app, dispute_closed_body(&format!("evt_cf1_{}", Uuid::new_v4().simple()), &du, "won", &pi, 6000));
    assert_eq!(rc.status(), StatusCode::OK);
    assert_eq!(dispute_row_count(&conn, &du).await, 1, "close-before-create seeded the row");
    assert_eq!(dispute_status(&conn, &du).await.as_deref(), Some("won"), "seeded directly terminal won");
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1, "debit applied");
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_reversal").await, 1, "reversal applied");
    assert_eq!(cash_collected(&conn, &inv).await, 6000, "net cash restored (won)");

    // The LATE created must reconcile to a no-op: it must NOT resurrect the dispute to open,
    // NOT add a second debit, NOT add a second row.
    let rcr = post_webhook!(app, dispute_created_body(&format!("evt_cf2_{}", Uuid::new_v4().simple()), &du, &pi, 6000));
    assert_eq!(rcr.status(), StatusCode::OK);
    assert_eq!(dispute_row_count(&conn, &du).await, 1, "still exactly one dispute row");
    assert_eq!(
        dispute_status(&conn, &du).await.as_deref(),
        Some("won"),
        "the late created did NOT resurrect the dispute to open"
    );
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1, "no second debit");
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_reversal").await, 1, "no second reversal");
    assert_eq!(cash_collected(&conn, &inv).await, 6000, "end state identical to in-order delivery");
}

// ───────────────────────────────────────────────────────────────────────────
// (i) ORDER-INDEPENDENCE vs. the LINKAGE (gap #26 dispute-vs-invoice.paid race, 0055):
//     `charge.dispute.created` delivered BEFORE the `invoice.paid` that writes the
//     pi_…→invoice linkage. Pre-fix the created acked "no_internal_invoice" and DROPPED
//     the dispute (no row, no debit, cap untightened) — and it was NOT self-healing (the
//     later invoice.paid never re-checked; a Stripe resend is dedup-acked). Post-fix the
//     created PARKS the dispute (recorded, no debit yet); the later invoice.paid promotes
//     it (billing_disputes row + dispute_debit + cap tightened). A redelivery of either
//     event is a no-op; both delivery orders converge identically.
// ───────────────────────────────────────────────────────────────────────────

/// Seed a FINALIZED infra invoice + its `ref_kind='invoice'` linkage WITHOUT driving
/// `invoice.paid` yet, and pre-pick the settling `pi_…` the eventual `invoice.paid` will
/// name. Yields `(internal_invoice_id, provider_invoice_id (in_…), pi_…)`. This lets a test
/// deliver a dispute on that `pi_…` BEFORE the linkage exists (the out-of-order case).
macro_rules! seed_finalized_unpaid_invoice {
    ($conn:expr, $creator:expr, $total:expr) => {{
        let inv = zeroship_core::typed_id::new_invoice_id();
        $conn
            .execute(
                "INSERT INTO zeroship.invoices \
                   (id, creator_id, period, status, subtotal_cents, credit_cents, tax_cents, \
                    total_cents, finalized_at) \
                 VALUES ($1, $2, $3::date, 'finalized', $4, 0, 0, $4, NOW())",
                &[&inv, &$creator, &this_period(), &($total as i64)],
            )
            .await
            .expect("finalized invoice");
        let provider_invoice = format!("in_dsp_{}", Uuid::new_v4().simple());
        $conn
            .execute(
                "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
                 VALUES ($1, 'stripe', 'invoice', $2)",
                &[&inv, &provider_invoice],
            )
            .await
            .expect("provider ref");
        let pi = format!("pi_dsp_{}", Uuid::new_v4().simple());
        (inv, provider_invoice, pi)
    }};
}

/// Drive the REAL `invoice.paid` webhook for an ALREADY-seeded finalized invoice, naming a
/// KNOWN settling `pi_…` (so a dispute can have referenced it before this paid arrives). This
/// records the charge row + the `pi_…`→invoice linkage AND (post-fix) promotes any parked
/// dispute matching that `pi_…`.
macro_rules! drive_invoice_paid_for {
    ($app:expr, $conn:expr, $creator:expr, $provider_invoice:expr, $pi:expr, $total:expr) => {{
        let cus = creator_customer(&$conn, $creator).await;
        let paid_body = json!({
            "id": format!("evt_paid_{}", Uuid::new_v4().simple()),
            "type": "invoice.paid",
            "created": 1_777_000_000i64,
            "data": { "object": {
                "id": $provider_invoice,
                "amount_paid": ($total as i64),
                "currency": "usd",
                "payment_intent": $pi,
                "customer": cus,
                "metadata": { "invoice_kind": "infra" }
            }}
        });
        let resp = post_webhook!($app, paid_body);
        assert_eq!(resp.status(), StatusCode::OK, "invoice.paid webhook must 200");
        resp
    }};
}

async fn pending_dispute_count(conn: &compio_postgres::Client, du: &str) -> i64 {
    conn.query(
        "SELECT COUNT(*)::bigint AS n FROM zeroship.pending_disputes WHERE provider_dispute_id = $1",
        &[&du],
    )
    .await
    .expect("count pending disputes")[0]
        .get::<_, i64>("n")
}

#[compio::test]
async fn dispute_created_before_invoice_paid_resolves_on_linkage() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "created-before-paid").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    let (inv, provider_invoice, pi) = seed_finalized_unpaid_invoice!(conn, creator, 6000);

    // (1) The dispute arrives FIRST — its pi_… has NO linkage yet. Pre-fix this dropped the
    // dispute; post-fix it PARKS it: no billing_disputes row, no debit, but a pending row.
    let du = format!("du_cbp_{}", Uuid::new_v4().simple());
    let r1 = post_webhook!(
        app,
        dispute_created_body(&format!("evt_cbp1_{}", Uuid::new_v4().simple()), &du, &pi, 6000)
    );
    assert_eq!(r1.status(), StatusCode::OK);
    let b1: Value = serde_json::from_slice(&test::read_body(r1).await).unwrap();
    assert_eq!(b1["status"], "dispute_pending_linkage", "created-before-paid is parked, not dropped");
    assert_eq!(dispute_row_count(&conn, &du).await, 0, "no billing_disputes row yet (unlinked)");
    assert_eq!(pending_dispute_count(&conn, &du).await, 1, "parked in pending_disputes");
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 0, "no debit applied yet");

    // (2) Now invoice.paid arrives and writes the pi_…→invoice linkage. It MUST promote the
    // parked dispute: a billing_disputes row (open) + the dispute_debit tightening the cap,
    // and the holding row is consumed.
    drive_invoice_paid_for!(app, conn, creator, provider_invoice, pi, 6000);
    assert_eq!(cash_collected(&conn, &inv).await, 0, "charge 6000 then dispute_debit -6000 = 0");
    assert_eq!(dispute_row_count(&conn, &du).await, 1, "the parked dispute was promoted");
    assert_eq!(dispute_status(&conn, &du).await.as_deref(), Some("open"));
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1, "debit now applied");
    assert_eq!(pending_dispute_count(&conn, &du).await, 0, "holding row consumed on promotion");

    // The promoted dispute carried the parked metadata through.
    let row = conn
        .query(
            "SELECT reason, evidence_due_at IS NOT NULL AS has_due, amount_cents \
               FROM zeroship.billing_disputes WHERE provider_dispute_id = $1",
            &[&du],
        )
        .await
        .expect("read promoted dispute")[0]
        .clone();
    assert_eq!(row.get::<_, Option<String>>("reason").as_deref(), Some("fraudulent"));
    assert!(row.get::<_, bool>("has_due"), "evidence_due_at carried through the park");
    assert_eq!(row.get::<_, i64>("amount_cents"), 6000);

    // (3) The over-refund cap reflects the dispute: a $50 cash refund (would have fit the
    // $60 pre-dispute cap) is now rejected; the dispute tightened it to $0.
    let mut rconn = side_conn(&url).await;
    let provider = NativeRefundProvider;
    let outcome = issue_refund(
        &mut rconn, &provider, &inv, 5000, 5000, 0, RefundDestination::Cash,
        Some("post-promotion over-refund"), &format!("idem-cbp-{}", Uuid::new_v4().simple()),
    )
    .await
    .expect("issue_refund call");
    assert!(
        matches!(outcome, RefundOutcome::OverRefund(_)),
        "the promoted dispute tightened the cap; a $50 refund must be rejected, got {outcome:?}"
    );

    // (4) Idempotency both ways: a redelivered created (post-promotion) is a no-op; a
    // redelivered invoice.paid is a no-op. No second row, no second debit.
    let r_redeliver_created = post_webhook!(
        app,
        dispute_created_body(&format!("evt_cbp2_{}", Uuid::new_v4().simple()), &du, &pi, 6000)
    );
    assert_eq!(r_redeliver_created.status(), StatusCode::OK);
    drive_invoice_paid_for!(app, conn, creator, provider_invoice, pi, 6000);
    assert_eq!(dispute_row_count(&conn, &du).await, 1, "still exactly one dispute row");
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1, "still exactly one debit");
    assert_eq!(pending_dispute_count(&conn, &du).await, 0, "no re-park");
}

/// A dispute on a charge the platform NEVER invoiced parks but NEVER resolves — it does NOT
/// poison the webhook (no 5xx-retry storm), and never invents a billing_disputes row.
#[compio::test]
async fn dispute_on_never_invoiced_charge_parks_without_poison() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "never-ours").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;

    // A pi_… that no invoice.paid will ever link (a Connect end-user charge we never invoiced).
    let pi = format!("pi_never_{}", Uuid::new_v4().simple());
    let du = format!("du_never_{}", Uuid::new_v4().simple());
    let r = post_webhook!(
        app,
        dispute_created_body(&format!("evt_never1_{}", Uuid::new_v4().simple()), &du, &pi, 4000)
    );
    // Acked 200 (parked) — NOT a 5xx that Stripe would retry into a storm.
    assert_eq!(r.status(), StatusCode::OK, "never-ours dispute is acked, not poisoned");
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "dispute_pending_linkage");
    assert_eq!(dispute_row_count(&conn, &du).await, 0, "no billing_disputes row invented");
    assert_eq!(pending_dispute_count(&conn, &du).await, 1, "parked, awaiting a linkage that never comes");

    // A redelivery is still a clean ack (idempotent park) — no row, no poison.
    let r2 = post_webhook!(
        app,
        dispute_created_body(&format!("evt_never2_{}", Uuid::new_v4().simple()), &du, &pi, 4000)
    );
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(pending_dispute_count(&conn, &du).await, 1, "still exactly one parked row");
    assert_eq!(dispute_row_count(&conn, &du).await, 0);
}

// ───────────────────────────────────────────────────────────────────────────
// (f) schema guard: the PR-8 objects exist on the migrated DB.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn pr8_schema_objects_present() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let conn = side_conn(&url).await;

    let tbl: i64 = conn
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_tables \
             WHERE schemaname = 'zeroship' AND tablename = 'billing_disputes'",
            &[],
        )
        .await
        .expect("table")[0]
        .get("n");
    assert_eq!(tbl, 1, "billing_disputes table must exist");

    let trg: i64 = conn
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_trigger \
             WHERE tgname = 'billing_disputes_controlled_update_trg'",
            &[],
        )
        .await
        .expect("trigger")[0]
        .get("n");
    assert_eq!(trg, 1, "billing_disputes controlled-update trigger must exist");

    let idx: i64 = conn
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_indexes \
             WHERE schemaname = 'zeroship' AND indexname = 'invoice_payments_dispute_provider_ref_key'",
            &[],
        )
        .await
        .expect("idx")[0]
        .get("n");
    assert_eq!(idx, 1, "the dispute payment-row dedup index must exist");

    // The dispute_status domain accepts open/won/lost and rejects junk.
    let ok: i64 = conn
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_type WHERE typname = 'dispute_status'",
            &[],
        )
        .await
        .expect("domain")[0]
        .get("n");
    assert_eq!(ok, 1, "dispute_status domain must exist");

    // 0055: the pending-dispute holding table (created-before-paid order-independence).
    let pending: i64 = conn
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_tables \
             WHERE schemaname = 'zeroship' AND tablename = 'pending_disputes'",
            &[],
        )
        .await
        .expect("pending table")[0]
        .get("n");
    assert_eq!(pending, 1, "pending_disputes holding table must exist");
}

// ───────────────────────────────────────────────────────────────────────────
// C1: the dispute money-write rail takes the SAME per-creator advisory lock every
//     sibling money path (refund/credit/void/proration) takes — so a dispute's
//     `dispute_debit` cannot interleave with a concurrent refund reading the cap
//     anchor (`cash = Σ(invoice_payments)`) pre-debit. We prove the lock by holding
//     the per-creator key on an observer txn and showing `record_dispute_created`
//     BLOCKS (its `lock_dispute_creator` waits on the held key) until we release it.
//
// RED pre-fix: `disputes.rs` took NO advisory lock, so `record_dispute_created`
//     would commit the dispute row WHILE the observer held the creator key — the
//     `timeout` would NOT elapse and the post-release assert that the row appears
//     "only after release" would be false (the row exists during the held window).
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_created_takes_per_creator_advisory_lock() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "dsp-lock").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    let (inv, pi) = seed_paid_invoice!(app, conn, creator, 9000);
    assert_eq!(cash_collected(&conn, &inv).await, 9000, "cash starts at the charge");

    // (1) An OBSERVER connection takes the per-creator advisory lock in an OPEN txn and
    // HOLDS it — exactly the key every sibling money path (and now the dispute rail) takes.
    let mut obs = side_conn(&url).await;
    let obs_tx = obs.transaction().await.expect("observer tx");
    obs_tx
        .execute(
            "SELECT pg_advisory_xact_lock(hashtext($1::text)::bigint)",
            &[&creator.to_string()],
        )
        .await
        .expect("observer takes the creator lock");

    // (2) On a DEDICATED connection, call the REAL dispute money-write. It must BLOCK on
    // `lock_dispute_creator` while the observer holds the key. A bounded `timeout` therefore
    // ELAPSES — the strongest faithful proof the path waits on the per-creator lock.
    let mut writer = side_conn(&url).await;
    let du = format!("du_lock_{}", Uuid::new_v4().simple());
    let blocked = compio::time::timeout(
        std::time::Duration::from_millis(1500),
        zeroship_control::disputes::record_dispute_created(
            &mut writer, &inv, 9000, "usd", Some("fraudulent"), None, &du,
        ),
    )
    .await;
    assert!(
        blocked.is_err(),
        "record_dispute_created must BLOCK on the held per-creator advisory lock — it returned \
         while the observer held the key, so the dispute rail took NO lock (the C1 bug)",
    );
    // While blocked, NOTHING was written (the txn is still waiting on the lock).
    assert_eq!(dispute_row_count(&conn, &du).await, 0, "no dispute row written while blocked");
    assert_eq!(cash_collected(&conn, &inv).await, 9000, "cash untouched while blocked");

    // A DIFFERENT creator's key is free — the lock serializes per creator only.
    let other = make_creator(&conn).await;
    let other_free: bool = conn
        .query(
            "SELECT pg_try_advisory_xact_lock(hashtext($1::text)::bigint) AS got",
            &[&other.to_string()],
        )
        .await
        .expect("try other")[0]
        .get("got");
    assert!(other_free, "a DIFFERENT creator's advisory lock is free — per-creator, not global");

    // Drop the blocked writer's connection so its abandoned (timed-out) txn can't race the
    // retry for the lock once it's released — the retry below owns the write deterministically.
    drop(writer);

    // (3) RELEASE the observer lock (commit the holding txn). The dispute write — re-driven on
    // a fresh connection now that the key is free — completes, writing the row + the debit.
    obs_tx.commit().await.expect("release observer lock");
    let mut writer2 = side_conn(&url).await;
    zeroship_control::disputes::record_dispute_created(
        &mut writer2, &inv, 9000, "usd", Some("fraudulent"), None, &du,
    )
    .await
    .expect("dispute write completes once the lock is free");

    assert_eq!(dispute_row_count(&conn, &du).await, 1, "the dispute row landed after release");
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1, "one dispute_debit");
    assert_eq!(
        cash_collected(&conn, &inv).await,
        0,
        "the dispute_debit tightened the cap by the full disputed amount (9000 − 9000)",
    );
    let _ = pi;
}

// ───────────────────────────────────────────────────────────────────────────
// BUG-4: a `charge.dispute.created` with amount>0 but NEITHER `payment_intent` NOR `charge`
// has NO settling object to resolve OR park against. `resolve_invoice_for_dispute` returns
// None and `park_pending_dispute` REJECTS (it requires a candidate) — so the pre-fix park
// branch 500'd, and Stripe would retry the malformed event FOREVER (a poison/retry storm).
// The fix ACKS 200 with a warn (matching every other unrecognized-input ignore path) and
// writes nothing.
//
// RED pre-fix: the handler reached `park_pending_dispute(no candidate) → Err → err_json(500)`,
// so `r.status()` is 500, not 200.
// ───────────────────────────────────────────────────────────────────────────

/// A `charge.dispute.created` with amount>0 but NO `payment_intent`/`charge` (the BUG-4
/// malformed shape). Stripe never actually sends this — purely defensive.
fn dispute_created_body_no_settling_object(evt: &str, du: &str, amount: i64) -> String {
    json!({
        "id": evt,
        "type": "charge.dispute.created",
        "created": 1_777_017_600i64,
        "data": { "object": {
            "id": du,
            "amount": amount,
            "currency": "usd",
            "status": "needs_response",
            "reason": "fraudulent"
        }}
    })
    .to_string()
}

#[compio::test]
async fn dispute_created_with_no_settling_object_is_acked_not_poisoned() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "no-settling-object").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;

    let du = format!("du_nosettle_{}", Uuid::new_v4().simple());
    let evt = format!("evt_nosettle_{}", Uuid::new_v4().simple());
    let r = post_webhook!(app, dispute_created_body_no_settling_object(&evt, &du, 4000));
    // ACKED 200 (not a 5xx that Stripe would retry into a storm).
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "a dispute with no pi_/ch_ must be acked 200, not 500-poisoned"
    );
    let b: Value = serde_json::from_slice(&test::read_body(r).await).unwrap();
    assert_eq!(b["status"], "dispute_no_settling_object");

    // Nothing written: no billing_disputes row, no pending_disputes row, no payment movement.
    assert_eq!(dispute_row_count(&conn, &du).await, 0, "no billing_disputes row invented");
    assert_eq!(pending_dispute_count(&conn, &du).await, 0, "no pending_disputes row parked");
}

// ───────────────────────────────────────────────────────────────────────────
// GAP #14: a `lost → won` illegal reorder (the financially riskier direction — a bug would
// spuriously RESTORE clawed-back cash via a stray `dispute_reversal`). Handler-level: dispute
// closed `lost`, then a late/replayed `won` → status stays `lost`, NO `dispute_reversal`
// appended, cash stays clawed-back at 0. The mirror of (g) but in the opposite direction.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_lost_then_late_won_is_rejected_cash_stays_clawed_back() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "lost-then-won").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    let (inv, pi) = seed_paid_invoice!(app, conn, creator, 6000);

    let du = format!("du_lw_{}", Uuid::new_v4().simple());
    // created → open, debit to 0.
    let r = post_webhook!(app, dispute_created_body(&format!("evt_lw1_{}", Uuid::new_v4().simple()), &du, &pi, 6000));
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(cash_collected(&conn, &inv).await, 0);

    // closed LOST → debit stands, cash stays 0, status lost, NO reversal.
    let rl = post_webhook!(app, dispute_closed_body(&format!("evt_lw2_{}", Uuid::new_v4().simple()), &du, "lost", &pi, 6000));
    assert_eq!(rl.status(), StatusCode::OK);
    assert_eq!(dispute_status(&conn, &du).await.as_deref(), Some("lost"));
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_reversal").await, 0, "lost adds no reversal");
    assert_eq!(cash_collected(&conn, &inv).await, 0, "lost leaves the debit standing");

    // A LATE / replayed closed WON must NOT flip the terminal lost dispute and must NOT append
    // a spurious reversal restoring cash the chargeback clawed back. The handler 200-acks
    // (a no-op via the WHERE status='open' gate + the trigger backstop).
    let rw = post_webhook!(app, dispute_closed_body(&format!("evt_lw3_{}", Uuid::new_v4().simple()), &du, "won", &pi, 6000));
    assert_eq!(rw.status(), StatusCode::OK, "the late won is acked, not 500");
    assert_eq!(
        dispute_status(&conn, &du).await.as_deref(),
        Some("lost"),
        "the lost→won reorder is rejected; status stays lost"
    );
    assert_eq!(
        payment_kind_count(&conn, &inv, "dispute_reversal").await,
        0,
        "no spurious dispute_reversal restoring clawed-back cash"
    );
    assert_eq!(
        cash_collected(&conn, &inv).await,
        0,
        "the clawed-back cash stays gone — a lost chargeback is final"
    );
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1, "exactly one debit");
}

// ───────────────────────────────────────────────────────────────────────────
// GAP #18: close-before-create LOST. The `.closed lost` arrives with no `.created` yet → a
// fresh terminal `lost` row is seeded DIRECTLY, the `dispute_debit` is applied (always), but
// NO `dispute_reversal` (lost ≠ won). Cash ends clawed-back. The lost twin of (h).
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_closed_lost_before_created_seeds_terminal_debit_only() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "close-lost-first").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    let (inv, pi) = seed_paid_invoice!(app, conn, creator, 6000);
    assert_eq!(cash_collected(&conn, &inv).await, 6000);

    let du = format!("du_clf_{}", Uuid::new_v4().simple());
    // closed LOST arrives FIRST (no created yet). Seed a terminal lost row applying ONLY the
    // debit (no reversal), resolving the invoice via the pi_….
    let rc = post_webhook!(app, dispute_closed_body(&format!("evt_clf1_{}", Uuid::new_v4().simple()), &du, "lost", &pi, 6000));
    assert_eq!(rc.status(), StatusCode::OK);
    let bc: Value = serde_json::from_slice(&test::read_body(rc).await).unwrap();
    assert_eq!(bc["dispute_status"], "lost");
    assert_eq!(dispute_row_count(&conn, &du).await, 1, "close-before-create seeded the row");
    assert_eq!(dispute_status(&conn, &du).await.as_deref(), Some("lost"), "seeded directly terminal lost");
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1, "debit applied");
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_reversal").await, 0, "NO reversal on lost");
    assert_eq!(cash_collected(&conn, &inv).await, 0, "net cash clawed back (lost)");

    // The LATE created reconciles to a no-op: no resurrect to open, no second debit/row.
    let rcr = post_webhook!(app, dispute_created_body(&format!("evt_clf2_{}", Uuid::new_v4().simple()), &du, &pi, 6000));
    assert_eq!(rcr.status(), StatusCode::OK);
    assert_eq!(dispute_row_count(&conn, &du).await, 1, "still exactly one dispute row");
    assert_eq!(dispute_status(&conn, &du).await.as_deref(), Some("lost"), "late created did NOT resurrect to open");
    assert_eq!(payment_kind_count(&conn, &inv, "dispute_debit").await, 1, "no second debit");
    assert_eq!(cash_collected(&conn, &inv).await, 0, "end state identical to in-order delivery");
}

// ───────────────────────────────────────────────────────────────────────────
// GAP #19: close-before-create with NO resolvable invoice (ctx=None). A `.closed` arrives for
// a du_… whose pi_/ch_ resolves to no invoice (an unrecorded charge) AND no dispute row exists
// → `record_dispute_closed` returns Ok(None) → handler acks `no_dispute_row`, writing NOTHING
// (no row invented, no cash moved).
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_closed_before_created_with_no_invoice_acks_no_dispute_row() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "close-no-invoice").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;

    // A pi_… that no invoice.paid ever linked (an unrecorded / Connect end-user charge).
    let pi = format!("pi_unlinked_{}", Uuid::new_v4().simple());
    let du = format!("du_noinv_{}", Uuid::new_v4().simple());
    let rc = post_webhook!(app, dispute_closed_body(&format!("evt_noinv1_{}", Uuid::new_v4().simple()), &du, "lost", &pi, 6000));
    assert_eq!(rc.status(), StatusCode::OK, "no-invoice close is acked, not 500");
    let bc: Value = serde_json::from_slice(&test::read_body(rc).await).unwrap();
    assert_eq!(bc["status"], "no_dispute_row", "ctx=None → no_dispute_row ack");

    // Nothing written.
    assert_eq!(dispute_row_count(&conn, &du).await, 0, "no billing_disputes row invented");
    assert_eq!(pending_dispute_count(&conn, &du).await, 0, "no pending_disputes row");
}

// ───────────────────────────────────────────────────────────────────────────
// GAP #16: directly EXERCISE the 0053 `billing_disputes_controlled_update` trigger RAISEs.
// The app layer always gates the close UPDATE with `WHERE status='open'`, so the illegal
// terminal→* transitions NEVER reach the DB through the app — a DROPPED or INVERTED trigger
// would currently pass the whole handler-level suite. Only a RAW direct UPDATE/DELETE
// exercises the trigger. We seed rows directly and assert the DB RAISEs on every illegal
// mutation: won→lost, won→open, lost→won, a frozen-money-column rewrite, and a DELETE.
//
// FAITHFUL / would-FAIL-if-removed: each raw statement below succeeds (no error) if the
// trigger is dropped or its guard inverted, so every `expect_err` flips to a pass → the test
// FAILS. (Proven by dropping the trigger on the dedicated DB — see the fix report.)
// ───────────────────────────────────────────────────────────────────────────

/// Seed a `billing_disputes` row DIRECTLY in a given terminal/open status (bypassing the
/// handler) so the raw-UPDATE trigger can be exercised against it. Returns the du_….
async fn seed_dispute_row_direct(
    conn: &compio_postgres::Client,
    inv: &str,
    status: &str,
    amount: i64,
) -> String {
    let dsp = zeroship_core::typed_id::new_dispute_id();
    let du = format!("du_trg_{}", Uuid::new_v4().simple());
    conn.execute(
        "INSERT INTO zeroship.billing_disputes \
           (id, invoice_id, amount_cents, currency, status, provider_dispute_id, resolved_at) \
         VALUES ($1, $2, $3, 'usd', $4::text::zeroship.dispute_status, $5, \
                 CASE WHEN $4 = 'open' THEN NULL ELSE NOW() END)",
        &[&dsp, &inv, &amount, &status, &du],
    )
    .await
    .expect("seed dispute row direct");
    du
}

#[compio::test]
async fn billing_disputes_controlled_update_trigger_raises_on_illegal_mutations() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "trigger").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;
    // Distinct periods so each seeded invoice fits the partial-unique-period index.
    let (inv_a, _pa) = seed_paid_invoice_period!(app, conn, creator, 6000, period_offset(0));
    let (inv_b, _pb) = seed_paid_invoice_period!(app, conn, creator, 6000, period_offset(1));
    let (inv_c, _pc) = seed_paid_invoice_period!(app, conn, creator, 6000, period_offset(2));

    // (1) won → lost is rejected (the classic stale-lost-after-won that would strand cash).
    let du_won = seed_dispute_row_direct(&conn, &inv_a, "won", 6000).await;
    let e = conn
        .execute(
            "UPDATE zeroship.billing_disputes SET status = 'lost'::zeroship.dispute_status \
             WHERE provider_dispute_id = $1",
            &[&du_won],
        )
        .await;
    assert!(e.is_err(), "won→lost MUST be rejected by the controlled-update trigger");
    assert_eq!(dispute_status(&conn, &du_won).await.as_deref(), Some("won"), "row unchanged");

    // (2) won → open (a terminal→open resurrection) is rejected.
    let e = conn
        .execute(
            "UPDATE zeroship.billing_disputes SET status = 'open'::zeroship.dispute_status \
             WHERE provider_dispute_id = $1",
            &[&du_won],
        )
        .await;
    assert!(e.is_err(), "won→open MUST be rejected");
    assert_eq!(dispute_status(&conn, &du_won).await.as_deref(), Some("won"));

    // (3) lost → won (the financially riskier restore) is rejected.
    let du_lost = seed_dispute_row_direct(&conn, &inv_b, "lost", 6000).await;
    let e = conn
        .execute(
            "UPDATE zeroship.billing_disputes SET status = 'won'::zeroship.dispute_status \
             WHERE provider_dispute_id = $1",
            &[&du_lost],
        )
        .await;
    assert!(e.is_err(), "lost→won MUST be rejected");
    assert_eq!(dispute_status(&conn, &du_lost).await.as_deref(), Some("lost"));

    // (4) a frozen-money-column rewrite (amount_cents) is rejected even on an OPEN row.
    let du_open = seed_dispute_row_direct(&conn, &inv_c, "open", 6000).await;
    let e = conn
        .execute(
            "UPDATE zeroship.billing_disputes SET amount_cents = 1 WHERE provider_dispute_id = $1",
            &[&du_open],
        )
        .await;
    assert!(e.is_err(), "rewriting the frozen amount_cents MUST be rejected");
    let amt: i64 = conn
        .query(
            "SELECT amount_cents FROM zeroship.billing_disputes WHERE provider_dispute_id = $1",
            &[&du_open],
        )
        .await
        .expect("read amount")[0]
        .get("amount_cents");
    assert_eq!(amt, 6000, "the frozen amount is unchanged");

    // (5) a DELETE is rejected outright (append-only fact).
    let e = conn
        .execute(
            "DELETE FROM zeroship.billing_disputes WHERE provider_dispute_id = $1",
            &[&du_open],
        )
        .await;
    assert!(e.is_err(), "DELETE on billing_disputes MUST be rejected");
    assert_eq!(dispute_row_count(&conn, &du_open).await, 1, "the row survives the rejected DELETE");

    // SANITY: a LEGAL open→won progression on the still-open row succeeds (proving the trigger
    // is not a blanket block — it permits the one allowed transition).
    conn.execute(
        "UPDATE zeroship.billing_disputes SET status = 'won'::zeroship.dispute_status, resolved_at = NOW() \
         WHERE provider_dispute_id = $1",
        &[&du_open],
    )
    .await
    .expect("legal open→won progression must succeed");
    assert_eq!(dispute_status(&conn, &du_open).await.as_deref(), Some("won"));
}

// ───────────────────────────────────────────────────────────────────────────
// GAP #22: `record_dispute_closed` (the close rail) takes the SAME per-creator advisory lock
// the create rail does. C1 was proven only on the CREATED rail (the existing
// `dispute_created_takes_per_creator_advisory_lock`); the close rail moves cash too — the won
// reversal (RAISES the cap) and the close-before-create debit (LOWERS it) must serialize
// against a concurrent refund. We hold the creator's `pg_advisory_xact_lock` key on an
// observer txn and assert `record_dispute_closed` BLOCKS until release, for BOTH the won path
// (existing-open-row) and the close-before-create path. A different creator's key is free.
//
// RED-if-removed: drop `lock_dispute_creator(&tx, inv)` from `record_dispute_closed` and the
// timeout would NOT elapse (the close commits while the observer holds the key) — `is_err()`
// flips false.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn dispute_closed_takes_per_creator_advisory_lock() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = Fixture::new(&url, "dsp-close-lock").await;
    let app = init_control!(fx);
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;

    // ---- WON path (existing open row): seed an open dispute, then close it WON under lock. ----
    let (inv_won, pi_won) = seed_paid_invoice_period!(app, conn, creator, 9000, period_offset(0));
    let du_won = format!("du_clk_won_{}", Uuid::new_v4().simple());
    let r = post_webhook!(app, dispute_created_body(&format!("evt_clk_w0_{}", Uuid::new_v4().simple()), &du_won, &pi_won, 9000));
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(cash_collected(&conn, &inv_won).await, 0, "debited to 0 on created");

    // Observer holds the per-creator key.
    let mut obs = side_conn(&url).await;
    let obs_tx = obs.transaction().await.expect("observer tx");
    obs_tx
        .execute(
            "SELECT pg_advisory_xact_lock(hashtext($1::text)::bigint)",
            &[&creator.to_string()],
        )
        .await
        .expect("observer takes the creator lock");

    // The won close (which appends a reversal RAISING the cap) must BLOCK on the held key.
    let mut writer = side_conn(&url).await;
    let ctx = zeroship_control::disputes::DisputeCloseContext {
        invoice_id: &inv_won,
        amount_cents: 9000,
        currency: "usd",
        reason: None,
    };
    let blocked = compio::time::timeout(
        std::time::Duration::from_millis(1500),
        zeroship_control::disputes::record_dispute_closed(
            &mut writer,
            &du_won,
            zeroship_control::disputes::DisputeStatus::Won,
            Some(ctx),
        ),
    )
    .await;
    assert!(
        blocked.is_err(),
        "record_dispute_closed (won) must BLOCK on the held per-creator advisory lock — the \
         close rail took NO lock if it returned while the observer held the key",
    );
    // While blocked, nothing moved: still terminal-pending, no reversal, cash still 0.
    assert_eq!(dispute_status(&conn, &du_won).await.as_deref(), Some("open"), "still open while blocked");
    assert_eq!(payment_kind_count(&conn, &inv_won, "dispute_reversal").await, 0, "no reversal while blocked");

    // A DIFFERENT creator's key is free — per-creator, not global.
    let other = make_creator(&conn).await;
    let other_free: bool = conn
        .query("SELECT pg_try_advisory_xact_lock(hashtext($1::text)::bigint) AS got", &[&other.to_string()])
        .await
        .expect("try other")[0]
        .get("got");
    assert!(other_free, "a DIFFERENT creator's advisory lock is free");

    // Drop the blocked writer so its abandoned txn can't race the retry for the key.
    drop(writer);
    // RELEASE and re-drive: the close now completes (reversal appended, cash restored).
    obs_tx.commit().await.expect("release observer lock");
    let mut writer2 = side_conn(&url).await;
    let ctx2 = zeroship_control::disputes::DisputeCloseContext {
        invoice_id: &inv_won, amount_cents: 9000, currency: "usd", reason: None,
    };
    zeroship_control::disputes::record_dispute_closed(
        &mut writer2, &du_won, zeroship_control::disputes::DisputeStatus::Won, Some(ctx2),
    )
    .await
    .expect("won close completes once the lock is free");
    assert_eq!(dispute_status(&conn, &du_won).await.as_deref(), Some("won"), "won after release");
    assert_eq!(payment_kind_count(&conn, &inv_won, "dispute_reversal").await, 1, "reversal applied");
    assert_eq!(cash_collected(&conn, &inv_won).await, 9000, "cash restored on won");

    // ---- CLOSE-BEFORE-CREATE path: no row yet; the terminal-seed debit LOWERS the cap. ----
    let (inv_cbc, pi_cbc) = seed_paid_invoice_period!(app, conn, creator, 7000, period_offset(1));
    let du_cbc = format!("du_clk_cbc_{}", Uuid::new_v4().simple());
    assert_eq!(cash_collected(&conn, &inv_cbc).await, 7000);

    let mut obs2 = side_conn(&url).await;
    let obs2_tx = obs2.transaction().await.expect("observer2 tx");
    obs2_tx
        .execute("SELECT pg_advisory_xact_lock(hashtext($1::text)::bigint)", &[&creator.to_string()])
        .await
        .expect("observer2 takes the creator lock");

    let mut writer3 = side_conn(&url).await;
    let ctx3 = zeroship_control::disputes::DisputeCloseContext {
        invoice_id: &inv_cbc, amount_cents: 7000, currency: "usd", reason: None,
    };
    let blocked2 = compio::time::timeout(
        std::time::Duration::from_millis(1500),
        zeroship_control::disputes::record_dispute_closed(
            &mut writer3, &du_cbc, zeroship_control::disputes::DisputeStatus::Lost, Some(ctx3),
        ),
    )
    .await;
    assert!(
        blocked2.is_err(),
        "record_dispute_closed (close-before-create) must BLOCK on the held per-creator lock",
    );
    assert_eq!(dispute_row_count(&conn, &du_cbc).await, 0, "no row seeded while blocked");
    assert_eq!(cash_collected(&conn, &inv_cbc).await, 7000, "cap untouched while blocked");

    drop(writer3);
    obs2_tx.commit().await.expect("release observer2 lock");
    let mut writer4 = side_conn(&url).await;
    let ctx4 = zeroship_control::disputes::DisputeCloseContext {
        invoice_id: &inv_cbc, amount_cents: 7000, currency: "usd", reason: None,
    };
    zeroship_control::disputes::record_dispute_closed(
        &mut writer4, &du_cbc, zeroship_control::disputes::DisputeStatus::Lost, Some(ctx4),
    )
    .await
    .expect("close-before-create completes once the lock is free");
    assert_eq!(dispute_status(&conn, &du_cbc).await.as_deref(), Some("lost"), "seeded terminal lost after release");
    assert_eq!(payment_kind_count(&conn, &inv_cbc, "dispute_debit").await, 1, "debit applied");
    assert_eq!(cash_collected(&conn, &inv_cbc).await, 0, "cap tightened by the close-before-create debit");
    let _ = pi_cbc;
}

// ───────────────────────────────────────────────────────────────────────────
// GAP #35: `resolve_invoice_for_dispute` deterministically PREFERS `payment_intent` over
// `charge` when both candidate ids map to a (different) invoice — the `ORDER BY (ref_kind =
// 'payment_intent') DESC LIMIT 1`. We seed a pi_… linked to invoice A and a ch_… linked to a
// DIFFERENT invoice B (pathological — they would normally point at the same invoice), pass
// BOTH as candidates, and assert the pi_…'s invoice (A) wins.
// ───────────────────────────────────────────────────────────────────────────

#[compio::test]
async fn resolve_invoice_for_dispute_prefers_payment_intent_over_charge() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let conn = side_conn(&url).await;
    let creator = make_creator(&conn).await;

    // Two distinct finalized invoices for distinct periods (partial-unique-period index).
    let inv_pi = zeroship_core::typed_id::new_invoice_id();
    let inv_ch = zeroship_core::typed_id::new_invoice_id();
    conn.execute(
        "INSERT INTO zeroship.invoices (id, creator_id, period, status, subtotal_cents, credit_cents, tax_cents, total_cents, finalized_at) \
         VALUES ($1, $2, $3::date, 'finalized', 6000, 0, 0, 6000, NOW())",
        &[&inv_pi, &creator, &period_offset(0)],
    ).await.expect("invoice A");
    conn.execute(
        "INSERT INTO zeroship.invoices (id, creator_id, period, status, subtotal_cents, credit_cents, tax_cents, total_cents, finalized_at) \
         VALUES ($1, $2, $3::date, 'finalized', 6000, 0, 0, 6000, NOW())",
        &[&inv_ch, &creator, &period_offset(1)],
    ).await.expect("invoice B");

    let pi = format!("pi_pref_{}", Uuid::new_v4().simple());
    let ch = format!("ch_pref_{}", Uuid::new_v4().simple());
    conn.execute(
        "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'payment_intent', $2)",
        &[&inv_pi, &pi],
    ).await.expect("pi linkage");
    conn.execute(
        "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
         VALUES ($1, 'stripe', 'charge', $2)",
        &[&inv_ch, &ch],
    ).await.expect("ch linkage");

    // Both candidates present → the payment_intent's invoice (A) must win, deterministically.
    let resolved = zeroship_control::disputes::resolve_invoice_for_dispute(&conn, &[&pi, &ch])
        .await
        .expect("resolve")
        .expect("an invoice resolves");
    assert_eq!(resolved, inv_pi, "payment_intent is preferred over charge");

    // Order of the candidate slice must NOT change the winner (the ORDER BY decides, not slice order).
    let resolved_rev = zeroship_control::disputes::resolve_invoice_for_dispute(&conn, &[&ch, &pi])
        .await
        .expect("resolve rev")
        .expect("an invoice resolves");
    assert_eq!(resolved_rev, inv_pi, "preference is by ref_kind, independent of candidate order");

    // And with ONLY the charge present, it falls back to the charge's invoice (B).
    let resolved_ch = zeroship_control::disputes::resolve_invoice_for_dispute(&conn, &[&ch])
        .await
        .expect("resolve ch")
        .expect("an invoice resolves");
    assert_eq!(resolved_ch, inv_ch, "charge alone resolves the charge's invoice");
}
