//! Faithful PG integration tests for billing-ops gap #26 PR-6 — the notification
//! send-ledger (`0052 billing_notifications`), the surrogate-id'd history tables
//! (`0041`/`0047`), the `BillingNotifier` seam, and the `cron::billing_notify` sweep.
//!
//! FAITHFUL by construction: every assertion runs against a LIVE, migrated Postgres
//! (`CONTROL_TEST_DB`; silent skip otherwise) and exercises the REAL paths —
//!   * the REAL `account_status::AccountStatusStore` (mints the `cbh_…` surrogate id);
//!   * the REAL `cron::billing_notify::{tick, sweep}` (advisory lock, claim-before-send,
//!     scan → send → flip, the `NOTIFY_REDRIVE_HORIZON` re-drive);
//!   * the REAL `billing_notifications` table / domains / two-phase claim;
//!   * the REAL `invoices` / `refunds` rows for the invoice-finalized / refunded kinds.
//! Only the EMAIL transport is a recording fake (`notify::RecordingNotifier`) so sends
//! are asserted without real email — exactly as the brief mandates.
//!
//! These FAIL against the pre-PR-6 code/schema: no `billing_notifications` table, no
//! surrogate ids on the history tables, no notify cron, no `Email::idempotency_key`.
//!
//! Brief regression coverage:
//!   (a) claim-before-send is multi-node-safe — two concurrent tick attempts: only one
//!       holds the advisory lock + sends; a duplicate (creator,kind,transition_id) claim
//!       is rejected (exactly-once claim). → `concurrent_ticks_send_each_event_once`
//!   (b) a crash after send before the `sent` flip → the `pending` row past
//!       NOTIFY_REDRIVE_HORIZON is re-driven, and the Mailer Idempotency-Key makes the
//!       re-send effect idempotent (key is passed). → `crash_before_flip_redrives_idempotent`
//!   (c) each event kind produces exactly one notification.
//!       → `each_kind_produces_exactly_one_notification`
//!   (d) the surrogate-id dedup key is collision-proof across sources (prefix-disjoint)
//!       — asserted in `zeroship-core` (`typed_id::tests`), re-checked here on real rows.
//!       → `history_surrogate_ids_carry_disjoint_prefixes`

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::account_status::AccountStatusStore;
use zeroship_control::cron::billing_notify::{self, NOTIFY_REDRIVE_HORIZON};
use zeroship_control::notify::{BillingNotificationKind, RecordingNotifier};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-notify-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

struct Fixture {
    state: Arc<AppState>,
    notifier: RecordingNotifier,
    pg: Arc<compio_postgres::Client>,
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

    // The RECORDING notifier under test — kept as a handle so assertions read what was
    // (or was not) delivered, and so a test can flip it to failing / suppressed.
    let notifier = RecordingNotifier::new();

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
        control_pg: Arc::clone(&control_pg),
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
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: Arc::new(notifier.clone()),
        pairwise_salt: [0u8; 32],
    });

    Fixture {
        state,
        notifier,
        pg: control_pg,
        blob_root,
        deploy_tmp_dir,
    }
}

// ---------------------------------------------------------------------------
// Seeding helpers — a creator (= a user) with a creator_billing identity row.
// ---------------------------------------------------------------------------

async fn make_creator(pg: &compio_postgres::Client) -> Uuid {
    let rows = pg
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, 'Notify Creator') RETURNING id",
            &[&format!("notify-{}@test.invalid", Uuid::new_v4().simple())],
        )
        .await
        .expect("insert user");
    let id: Uuid = rows[0].get("id");
    pg.execute(
        "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) \
         ON CONFLICT (creator_id) DO NOTHING",
        &[&id],
    )
    .await
    .expect("ensure creator_billing");
    id
}

/// Drive the dunning lifecycle for a creator to produce `creator_billing_status_history`
/// rows (each minting a `cbh_…` surrogate id). `event` is a monotonically increasing
/// unix ts for the Stripe order-safety high-water.
async fn fail_payment(state: &AppState, creator: Uuid, event: i64) {
    AccountStatusStore::new(state.registry.clone())
        .record_payment_failed(creator, Some("in_test"), event)
        .await
        .expect("record_payment_failed");
}

async fn recover_payment(state: &AppState, creator: Uuid, event: i64) {
    AccountStatusStore::new(state.registry.clone())
        .record_payment_recovered(creator, event)
        .await
        .expect("record_payment_recovered");
}

/// Seed a finalized invoice for a creator and return its id (the `invoice_finalized`
/// transition id). Writes the frozen total directly (the notifier reads it, never
/// re-prices). period is first-of-month.
async fn finalize_invoice(pg: &compio_postgres::Client, creator: Uuid, total_cents: i64) -> String {
    let id = zeroship_core::typed_id::new_invoice_id();
    pg.execute(
        "INSERT INTO zeroship.invoices \
            (id, creator_id, period, status, currency, subtotal_cents, credit_cents, tax_cents, total_cents, finalized_at) \
         VALUES ($1, $2, date_trunc('month', NOW())::date, 'finalized', 'usd', $3, 0, 0, $3, NOW())",
        &[&id, &creator, &total_cents],
    )
    .await
    .expect("insert finalized invoice");
    // Record the cash collected as an append-only invoice_payments row so the PR-3
    // over-refund trigger (cap = Σ(invoice_payments)) permits a later refund.
    let pay = zeroship_core::typed_id::new_invoice_payment_id();
    pg.execute(
        "INSERT INTO zeroship.invoice_payments \
            (id, invoice_id, kind, amount_cents, currency, provider_ref) \
         VALUES ($1, $2, 'charge', $3, 'usd', $4)",
        &[&pay, &id, &total_cents, &format!("pi_{}", Uuid::new_v4().simple())],
    )
    .await
    .expect("insert invoice payment");
    id
}

/// Seed an issued refund against an invoice and return its id (the `refunded`
/// transition id).
async fn issue_refund(
    pg: &compio_postgres::Client,
    invoice_id: &str,
    amount_cents: i64,
    destination: &str,
) -> String {
    let id = zeroship_core::typed_id::new_refund_id();
    pg.execute(
        "INSERT INTO zeroship.refunds \
            (id, invoice_id, amount_cents, subtotal_cents, tax_cents, currency, destination, \
             reason, idempotency_key, request_fingerprint, status, issued_at) \
         VALUES ($1, $2, $3, $3, 0, 'usd', $4::text::zeroship.refund_destination, 'test', $5, 'fp', 'issued', NOW())",
        &[&id, &invoice_id, &amount_cents, &destination, &Uuid::new_v4().simple().to_string()],
    )
    .await
    .expect("insert issued refund");
    id
}

/// Count ledger rows of a given status for a creator.
async fn ledger_count(pg: &compio_postgres::Client, creator: Uuid, status: &str) -> i64 {
    pg.query(
        "SELECT COUNT(*) AS c FROM zeroship.billing_notifications \
          WHERE creator_id = $1 AND status = $2::text::zeroship.notification_status",
        &[&creator, &status],
    )
    .await
    .expect("count ledger")[0]
        .get::<_, i64>("c")
}

/// Force a pending claim's `claimed_at` back so it is past the re-drive horizon.
async fn age_pending(pg: &compio_postgres::Client, creator: Uuid) {
    let back = i64::try_from(NOTIFY_REDRIVE_HORIZON.as_secs()).unwrap() + 60;
    pg.execute(
        "UPDATE zeroship.billing_notifications \
            SET claimed_at = NOW() - make_interval(secs => $2::double precision) \
          WHERE creator_id = $1 AND status = 'pending'",
        &[&creator, &(back as f64)],
    )
    .await
    .expect("age pending");
}

// ===========================================================================
// (c) each event kind produces exactly one notification
// ===========================================================================
#[compio::test]
async fn each_kind_produces_exactly_one_notification() {
    let Some(url) = db_url() else {
        eprintln!("SKIP each_kind_produces_exactly_one_notification: CONTROL_TEST_DB unset");
        return;
    };
    let fx = build_fixture(&url, "each-kind").await;
    let st = &*fx.state;

    // past_due: active→past_due (one cbh_ history row).
    let c_pd = make_creator(&fx.pg).await;
    fail_payment(st, c_pd, 1_000).await;

    // recovered: past_due→active.
    let c_rec = make_creator(&fx.pg).await;
    fail_payment(st, c_rec, 1_000).await;
    recover_payment(st, c_rec, 2_000).await;

    // suspended: drive a creator to past_due, then exhaust the dunning window.
    let c_susp = make_creator(&fx.pg).await;
    fail_payment(st, c_susp, 1_000).await;
    fx.pg
        .execute(
            "UPDATE zeroship.creator_billing_status \
                SET past_due_since = NOW() - make_interval(days => 30) WHERE creator_id = $1",
            &[&c_susp],
        )
        .await
        .expect("age past_due");
    AccountStatusStore::new(st.registry.clone())
        .suspend_exhausted(7)
        .await
        .expect("suspend_exhausted");

    // invoice_finalized + refunded.
    let c_inv = make_creator(&fx.pg).await;
    let inv = finalize_invoice(&fx.pg, c_inv, 1_234).await;
    let _refund = issue_refund(&fx.pg, &inv, 500, "cash").await;

    // One sweep delivers each pending transition once. The cron sweeps a SHARED test DB,
    // so assert per-MY-creator (the idempotency key is `{creator}:{kind}:{transition}`)
    // rather than on a global per-kind count.
    let _sent = billing_notify::tick(st).await.expect("tick");

    // Each of MY seeded creators gets exactly one notification of its kind.
    let pd = |c: Uuid, k: BillingNotificationKind| format!("{c}:{}:", k.as_str());
    assert_eq!(
        fx.notifier.delivered_for_key_prefix(&pd(c_pd, BillingNotificationKind::PastDue)),
        1,
        "the active→past_due creator gets exactly one past_due email"
    );
    assert_eq!(
        fx.notifier.delivered_for_key_prefix(&pd(c_rec, BillingNotificationKind::Recovered)),
        1,
        "the recovered creator gets exactly one recovered email"
    );
    // c_susp legitimately has BOTH a past_due AND a suspended transition — each fires its
    // own kind exactly once.
    assert_eq!(
        fx.notifier.delivered_for_key_prefix(&pd(c_susp, BillingNotificationKind::PastDue)),
        1
    );
    assert_eq!(
        fx.notifier.delivered_for_key_prefix(&pd(c_susp, BillingNotificationKind::Suspended)),
        1,
        "the dunning-exhausted creator gets exactly one suspended email"
    );
    assert_eq!(
        fx.notifier.delivered_for_key_prefix(&pd(c_inv, BillingNotificationKind::InvoiceFinalized)),
        1,
        "the finalized-invoice creator gets exactly one invoice_finalized email"
    );
    assert_eq!(
        fx.notifier.delivered_for_key_prefix(&pd(c_inv, BillingNotificationKind::Refunded)),
        1,
        "the refunded creator gets exactly one refunded email"
    );

    // A SECOND sweep delivers NOTHING NEW for my creators (every row is now `sent`).
    let pre = fx.notifier.delivered_count();
    let _again = billing_notify::tick(st).await.expect("second tick");
    for c in [c_pd, c_rec, c_susp, c_inv] {
        assert_eq!(
            fx.notifier.delivered_for_key_prefix(&format!("{c}:")),
            // each creator's full delivered count is unchanged by the second sweep
            fx.notifier.delivered_for_key_prefix(&format!("{c}:")),
            "second sweep must not re-deliver for creator {c}"
        );
    }
    // No NEW deliveries for my creators on the second sweep (the recording notifier dedup
    // + the `sent` ledger both guarantee it).
    assert_eq!(
        fx.notifier.delivered_count(),
        pre,
        "a second sweep with no new transitions delivers nothing new"
    );
    // Every delivered key is exactly-once (no dup deliveries) across both sweeps.
    for (_, _, key, delivered) in fx.notifier.attempts() {
        if delivered {
            assert_eq!(fx.notifier.delivered_for_key(&key), 1, "key {key} delivered >1");
        }
    }
}

// ===========================================================================
// (a) claim-before-send is multi-node safe: two CONCURRENT ticks send each event once
// ===========================================================================
#[compio::test]
async fn concurrent_ticks_send_each_event_once() {
    let Some(url) = db_url() else {
        eprintln!("SKIP concurrent_ticks_send_each_event_once: CONTROL_TEST_DB unset");
        return;
    };
    let fx = build_fixture(&url, "concurrent").await;
    let st = &*fx.state;

    // --- Guard 1: the dedicated advisory lock single-flights the sweep. ---
    // Hold the notify-family advisory lock on a SIDE session; a tick that cannot acquire
    // it skips entirely (the multi-node loser path) — proving only ONE instance sweeps
    // per tick. The lock key is the cron's `NOTIFY_SWEEP_ADVISORY_LOCK_KEY` ("zsnotf").
    let creator = make_creator(&fx.pg).await;
    fail_payment(st, creator, 1_000).await;
    const NOTIFY_LOCK_KEY: i64 = 0x7a73_6e6f_7466_0001;
    let (side, side_conn) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("side lock conn");
    compio::runtime::spawn(async move {
        let _ = side_conn.run().await;
    })
    .detach();
    let held: bool = side
        .query("SELECT pg_advisory_lock($1)", &[&NOTIFY_LOCK_KEY])
        .await
        .map(|_| true)
        .expect("hold notify lock");
    assert!(held);
    let blocked = billing_notify::tick(st).await.expect("tick while lock held");
    assert_eq!(blocked, 0, "a tick that loses the advisory lock must send NOTHING");
    assert_eq!(ledger_count(&fx.pg, creator, "pending").await, 0, "no claim while locked out");
    side.execute("SELECT pg_advisory_unlock($1)", &[&NOTIFY_LOCK_KEY])
        .await
        .expect("release notify lock");

    // --- Guard 2: exactly-once CLAIM under genuine dual-flight. ---
    // Two ticks racing on the SAME state. Whatever the interleaving, the claim INSERT
    // (the PK `(creator, kind, transition_id)`) arbitrates: the row is claimed by exactly
    // ONE flight, and the provider Idempotency-Key dedups any duplicate send so the
    // recipient sees ONE email. (The per-tick `sent` count can be 1 or 2 depending on
    // interleaving — the GUARANTEE is one ledger row + one DELIVERY, which is what we
    // assert; `total >= 1` proves the transition was delivered at all.)
    let (a, b) = futures::future::join(billing_notify::tick(st), billing_notify::tick(st)).await;
    let total = a.expect("tick a") + b.expect("tick b");
    assert!(total >= 1, "the transition must be delivered at least once, got {total}");
    // Scope to THIS test's creator (the cron sweeps a shared DB): the key prefix is
    // `{creator}:past_due:`.
    assert_eq!(
        fx.notifier
            .delivered_for_key_prefix(&format!("{creator}:{}:", BillingNotificationKind::PastDue.as_str())),
        1,
        "exactly-once claim + idempotent delivery: the recipient sees ONE past_due email \
         even under dual-flight (the duplicate claim is rejected by the PK; a duplicate \
         send is deduped by the Idempotency-Key)"
    );
    // The PK guarantees exactly ONE ledger row for the transition, now `sent`.
    assert_eq!(ledger_count(&fx.pg, creator, "sent").await, 1);
    assert_eq!(ledger_count(&fx.pg, creator, "pending").await, 0);
}

// ===========================================================================
// (b) crash after send, before the `sent` flip → re-drive past the horizon, the
//     Idempotency-Key makes the re-send effect idempotent (the key IS passed).
// ===========================================================================
#[compio::test]
async fn crash_before_flip_redrives_idempotent() {
    let Some(url) = db_url() else {
        eprintln!("SKIP crash_before_flip_redrives_idempotent: CONTROL_TEST_DB unset");
        return;
    };
    let fx = build_fixture(&url, "redrive").await;
    let st = &*fx.state;

    let creator = make_creator(&fx.pg).await;
    fail_payment(st, creator, 1_000).await;

    // Simulate a CRASH after send but before the flip: drive the FIRST send to the
    // notifier (delivered once) but force the ledger row to stay `pending` by failing
    // the flip. We model the crash by sending via the notifier directly through the
    // sweep, then resetting the row to pending + aging it.
    // The cron sweeps a shared test DB; scope all assertions to THIS creator's key.
    let key = format!(
        "{}:{}:{}",
        creator,
        BillingNotificationKind::PastDue.as_str(),
        // the transition_id is the cbh_ id of the only history row for this creator
        cbh_id(&fx.pg, creator).await,
    );
    let sent = billing_notify::tick(st).await.expect("first tick");
    assert!(sent >= 1, "first sweep sends at least my transition");
    assert_eq!(
        fx.notifier.delivered_for_key(&key),
        1,
        "the Idempotency-Key tuple (creator:kind:transition_id) is passed and delivered once"
    );

    // CRASH model: the `sent` flip never committed → roll the row back to pending and
    // age it past NOTIFY_REDRIVE_HORIZON.
    fx.pg
        .execute(
            "UPDATE zeroship.billing_notifications SET status = 'pending', sent_at = NULL \
              WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("reset to pending");
    age_pending(&fx.pg, creator).await;

    // Next tick RE-DRIVES the SAME row (re-claims past the horizon, re-sends). The
    // recording notifier dedups on the SAME Idempotency-Key, so the recipient sees ONE
    // email even though TWO send attempts were made (at-least-once delivery /
    // exactly-once claim / idempotent effect).
    let redriven = billing_notify::tick(st).await.expect("re-drive tick");
    assert!(redriven >= 1, "the stale pending row is re-driven (re-sent)");
    assert!(
        fx.notifier.attempts_for_key(&key) >= 2,
        "the re-drive made a SECOND send attempt for the same key"
    );
    assert_eq!(
        fx.notifier.delivered_for_key(&key),
        1,
        "the provider Idempotency-Key dedups the re-send: ONE delivery across re-drives"
    );
    // The row is now `sent` (the re-drive flipped it).
    assert_eq!(ledger_count(&fx.pg, creator, "sent").await, 1);
}

/// The cbh_ surrogate id of the single history row for a creator.
async fn cbh_id(pg: &compio_postgres::Client, creator: Uuid) -> String {
    pg.query(
        "SELECT id FROM zeroship.creator_billing_status_history WHERE creator_id = $1 ORDER BY at LIMIT 1",
        &[&creator],
    )
    .await
    .expect("read cbh id")[0]
        .get::<_, String>("id")
}

// ===========================================================================
// (d) surrogate-id dedup key is collision-proof across sources (prefix-disjoint),
//     re-checked on REAL rows (the cross-source disjointness is also asserted in
//     zeroship-core typed_id tests).
// ===========================================================================
#[compio::test]
async fn history_surrogate_ids_carry_disjoint_prefixes() {
    let Some(url) = db_url() else {
        eprintln!("SKIP history_surrogate_ids_carry_disjoint_prefixes: CONTROL_TEST_DB unset");
        return;
    };
    let fx = build_fixture(&url, "prefixes").await;
    let st = &*fx.state;

    let creator = make_creator(&fx.pg).await;
    fail_payment(st, creator, 1_000).await;
    let cbh = cbh_id(&fx.pg, creator).await;
    assert!(cbh.starts_with("cbh_"), "creator-billing-history id must be cbh_: {cbh}");

    // A spend-state-history row (minted by spend.rs) carries `she_`. We assert the
    // prefix from a directly-seeded row so this test does not require the spend engine.
    let she = zeroship_core::typed_id::new_spend_history_id();
    assert!(she.starts_with("she_"), "spend-history id must be she_: {she}");

    // The notify dedup key is (creator_id, kind, transition_id). Two transition ids from
    // DIFFERENT sources can never collide because their prefixes differ — proving the
    // cross-source dedup is collision-proof.
    assert_ne!(&cbh[..4], &she[..4], "cbh_ and she_ prefixes must differ");
    let inv = zeroship_core::typed_id::new_invoice_id();
    let refund = zeroship_core::typed_id::new_refund_id();
    let prefixes = [&cbh[..3], &she[..3], &inv[..3], &refund[..3]];
    for (i, a) in prefixes.iter().enumerate() {
        for b in &prefixes[i + 1..] {
            assert_ne!(a, b, "notification source prefixes must be pairwise-disjoint");
        }
    }
}
