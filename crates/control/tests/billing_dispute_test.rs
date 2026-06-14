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
}
