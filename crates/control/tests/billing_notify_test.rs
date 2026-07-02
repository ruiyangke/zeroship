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
use zeroship_control::account_status::DEFAULT_MAX_DUNNING_DAYS;
use zeroship_control::cron::billing_notify::NOTIFY_REDRIVE_HORIZON;
use zeroship_control::cron::{billing_notify, dunning, spend_reconcile};
use zeroship_control::notify::{BillingNotificationKind, RecordingNotifier};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// Process-global gate serializing the act of SWEEPING across this binary's tests.
///
/// The notify cron is FLEET-WIDE by design: one tick scans ALL creators and sends every
/// pending row through the `AppState.notifier` of whichever fixture drove that tick. Under
/// the default parallel runner that means a sibling test's tick can deliver MY creator's
/// email into the SIBLING's `RecordingNotifier` and flip the shared-DB row to `sent` —
/// invisible to my recorder. The DB ledger is immune (assert there where we can), but the
/// re-drive test must observe TWO send attempts for the SAME key on ITS OWN recorder to
/// prove the provider-side idempotency dedup, which only holds if no sibling steals the
/// row mid-sequence. This gate makes the SWEEP single-threaded across the binary's tests
/// WITHOUT weakening the cron: every gated tick still acquires the real advisory lock,
/// claims-before-send, and sends for real — only concurrent sibling SWEEPS are excluded,
/// exactly as a real fleet has at most one sweeper per tick (the cron's own invariant).
static TICK_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the sweep gate for a critical region that drives ticks. `lock().unwrap_or_else`
/// recovers a poisoned gate (a sibling test panicking mid-sweep must not cascade-fail the
/// rest) so an unrelated failure does not mask the result under test.
fn lock_tick_gate() -> std::sync::MutexGuard<'static, ()> {
    TICK_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
        notifier: Arc::new(notifier.clone()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: std::sync::Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
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

/// Count `sent` ledger rows for a creator of a given kind. This is the AUTHORITATIVE,
/// cross-fixture-safe record of "a notification was claimed AND delivered" — the cron
/// only flips a row to `sent` AFTER a successful (or suppressed/no-recipient) send. We
/// assert delivery off the shared DB ledger (per-creator, per-kind) rather than off the
/// per-fixture `RecordingNotifier`, because the cron is FLEET-WIDE: under the default
/// parallel runner, whichever test's tick wins the global advisory lock sends ALL
/// creators' pending rows through ITS OWN recorder — so a sibling's tick can deliver MY
/// creator's email and record it in the sibling's recorder, never mine. The ledger row,
/// keyed by my creator_id + kind, is immune to that and is the real artifact under test.
async fn sent_count_kind(
    pg: &compio_postgres::Client,
    creator: Uuid,
    kind: BillingNotificationKind,
) -> i64 {
    pg.query(
        "SELECT COUNT(*) AS c FROM zeroship.billing_notifications \
          WHERE creator_id = $1 AND status = 'sent' \
            AND kind = $2::text::zeroship.billing_notification_kind",
        &[&creator, &kind.as_str()],
    )
    .await
    .expect("count sent-by-kind")[0]
        .get::<_, i64>("c")
}

/// Drive `billing_notify::tick` until every `(creator, expected_sent)` in `want` has at
/// least `expected_sent` `sent` ledger rows AND zero `pending` rows — i.e. THIS test's
/// own transitions are fully delivered.
///
/// WHY a loop and not a single `tick`: the cron's single-flight advisory lock is
/// FLEET-WIDE (one global key). Under the DEFAULT parallel test runner a sibling test's
/// concurrent `tick` (or its held side-lock) can own the lock when this test ticks, so a
/// given `tick` legitimately wins nothing and returns 0 — exactly the multi-node
/// "loser skips this tick" path the production cron retries on its next ~5min cycle. We
/// reproduce that retry here so the assertion is scoped to THIS creator's settled state,
/// never to a global per-tick send count. The cron, the claim-before-send, and the lock
/// are exercised UNCHANGED — only the test waits out lock contention instead of assuming
/// one tick wins. The bound keeps a genuine bug (rows that never settle) from hanging.
async fn tick_until_sent(state: &AppState, pg: &compio_postgres::Client, want: &[(Uuid, i64)]) {
    for _ in 0..200 {
        {
            let _gate = lock_tick_gate();
            billing_notify::tick(state).await.expect("tick");
        }
        let mut all_settled = true;
        for &(creator, expected_sent) in want {
            let sent = ledger_count(pg, creator, "sent").await;
            let pending = ledger_count(pg, creator, "pending").await;
            if sent < expected_sent || pending > 0 {
                all_settled = false;
                break;
            }
        }
        if all_settled {
            return;
        }
    }
    panic!("tick_until_sent did not settle creators {want:?} within the retry bound");
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

    // Drive the cron until each of MY creators' transitions are delivered. The cron sweeps
    // a SHARED test DB and single-flights on a FLEET-WIDE advisory lock, so a single tick
    // can lose the lock to a concurrent sibling test and win nothing for my creators — we
    // tick until MY creators settle (the production cron's next-cycle retry), then assert
    // per-MY-creator (the idempotency key is `{creator}:{kind}:{transition}`) rather than
    // on any global per-kind or global-total count that a sibling could perturb.
    //   c_pd:   1 transition (past_due)
    //   c_rec:  2 (active→past_due AND past_due→active)
    //   c_susp: 2 (past_due AND suspended)
    //   c_inv:  2 (invoice_finalized AND refunded)
    tick_until_sent(
        st,
        &fx.pg,
        &[(c_pd, 1), (c_rec, 2), (c_susp, 2), (c_inv, 2)],
    )
    .await;

    // Each of MY seeded creators gets exactly one notification of its kind. We assert off
    // the shared DB LEDGER (cross-fixture-safe; see `sent_count_kind`) — exactly one `sent`
    // row of the kind for my creator — not off the per-fixture recorder, since a sibling's
    // tick can deliver my creator's email into the sibling's recorder.
    use BillingNotificationKind::{InvoiceFinalized, PastDue, Recovered, Refunded, Suspended};
    assert_eq!(
        sent_count_kind(&fx.pg, c_pd, PastDue).await,
        1,
        "the active→past_due creator gets exactly one past_due notification"
    );
    assert_eq!(
        sent_count_kind(&fx.pg, c_rec, Recovered).await,
        1,
        "the recovered creator gets exactly one recovered notification"
    );
    // c_susp legitimately has BOTH a past_due AND a suspended transition — each fires its
    // own kind exactly once.
    assert_eq!(sent_count_kind(&fx.pg, c_susp, PastDue).await, 1);
    assert_eq!(
        sent_count_kind(&fx.pg, c_susp, Suspended).await,
        1,
        "the dunning-exhausted creator gets exactly one suspended notification"
    );
    assert_eq!(
        sent_count_kind(&fx.pg, c_inv, InvoiceFinalized).await,
        1,
        "the finalized-invoice creator gets exactly one invoice_finalized notification"
    );
    assert_eq!(
        sent_count_kind(&fx.pg, c_inv, Refunded).await,
        1,
        "the refunded creator gets exactly one refunded notification"
    );

    // A SECOND sweep delivers NOTHING NEW for MY creators: every ledger row is `sent`, none
    // `pending`, and the per-(creator,kind) `sent` counts are unchanged. Read off the
    // ledger (per-creator) — a global recorder count would move if this tick swept a
    // sibling's freshly-pending rows in the shared DB.
    let pre: Vec<(Uuid, BillingNotificationKind, i64)> = {
        let mut v = Vec::new();
        for (c, k) in [
            (c_pd, PastDue),
            (c_rec, Recovered),
            (c_susp, PastDue),
            (c_susp, Suspended),
            (c_inv, InvoiceFinalized),
            (c_inv, Refunded),
        ] {
            v.push((c, k, sent_count_kind(&fx.pg, c, k).await));
        }
        v
    };
    {
        let _gate = lock_tick_gate();
        billing_notify::tick(st).await.expect("second tick");
    }
    for (c, k, before) in pre {
        assert_eq!(
            sent_count_kind(&fx.pg, c, k).await,
            before,
            "second sweep must not produce a new {k:?} sent row for creator {c}"
        );
        assert_eq!(
            ledger_count(&fx.pg, c, "pending").await,
            0,
            "no pending rows remain for creator {c} after settle"
        );
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
    // recipient sees ONE email.
    //
    // The dual-flight join below exercises the REAL race + PK arbitration. But the cron's
    // single-flight lock is FLEET-WIDE, so under the default parallel runner BOTH racing
    // ticks can lose the lock to a concurrent sibling test and win nothing this round —
    // the multi-node loser path. So we do NOT assert on the per-tick send count of this
    // one race (it can be 0, 1, or 2 depending on lock ownership); instead we run the
    // genuine race and THEN drain until MY creator settles. The exactly-once GUARANTEE is
    // unchanged: no matter how many ticks (racing or retried) touch this transition, the
    // PK admits exactly ONE ledger row and the Idempotency-Key dedups to ONE delivery.
    let (a, b) = futures::future::join(billing_notify::tick(st), billing_notify::tick(st)).await;
    a.expect("tick a");
    b.expect("tick b");
    // Drain MY creator's single past_due transition to `sent` (retrying past any sibling
    // lock contention — the production cron's next-cycle retry).
    tick_until_sent(st, &fx.pg, &[(creator, 1)]).await;

    // Exactly-once claim + delivery, asserted off the shared DB ledger scoped to THIS
    // creator (cross-fixture-safe): the PK `(creator, kind, transition_id)` admits exactly
    // ONE row for the past_due transition no matter how many racing/retried ticks touch it,
    // and the cron only flips it to `sent` after a successful send — so exactly one `sent`
    // past_due row for my creator proves "the recipient sees ONE past_due email."
    assert_eq!(
        sent_count_kind(&fx.pg, creator, BillingNotificationKind::PastDue).await,
        1,
        "exactly-once claim + idempotent delivery: ONE past_due notification even under \
         dual-flight (the duplicate claim is rejected by the PK; a duplicate send is \
         deduped by the Idempotency-Key)"
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

    // This test is the ONE that must observe its OWN recorder across the whole
    // seed → first-send → crash → re-drive sequence (the provider-dedup proof = TWO
    // attempts / ONE delivery on MY key). A sibling tick that swept MY pending row would
    // record the send in the sibling's recorder and flip my row in the shared DB, stealing
    // the observation. So we hold the fleet-wide sweep gate for the entire critical region
    // — INCLUDING the seeding, so no sibling can sweep my freshly-seeded row before MY tick
    // sends it. Every tick below is still a REAL gated sweep (advisory lock +
    // claim-before-send + send); only concurrent sibling sweeps are excluded, matching the
    // cron's own "one sweeper per tick" invariant, made deterministic for the binary.
    let _gate = lock_tick_gate();

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

    // FIRST send: drive (gated) ticks until MY transition is delivered+settled. With the
    // gate held, no sibling sweeps, so THIS fixture's notifier makes the send.
    let mut settled = false;
    for _ in 0..200 {
        billing_notify::tick(st).await.expect("first tick");
        if ledger_count(&fx.pg, creator, "sent").await >= 1
            && ledger_count(&fx.pg, creator, "pending").await == 0
        {
            settled = true;
            break;
        }
    }
    assert!(settled, "first sweep settled MY transition to sent");
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

    // The next sweep RE-DRIVES the SAME row (re-claims past the horizon, re-sends). The
    // recording notifier dedups on the SAME Idempotency-Key, so the recipient sees ONE
    // email even though TWO send attempts were made (at-least-once delivery / exactly-once
    // claim / idempotent effect). (Gate still held — no sibling can steal the re-drive.)
    let mut redrove = false;
    for _ in 0..200 {
        billing_notify::tick(st).await.expect("re-drive tick");
        if fx.notifier.attempts_for_key(&key) >= 2 {
            redrove = true;
            break;
        }
    }
    assert!(redrove, "the stale pending row is re-driven (a SECOND send attempt for the key)");
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

// ===========================================================================
// (#10) SPEND-BAND notifications — the spend_state_history → billing_notifications
// wiring. Drives a REAL spend transition (Allow→Warn→Degrade→Block AND a deadband
// HOLD) through the REAL spend_reconcile cron on live PG, then runs the REAL notify
// cron and asserts EXACTLY ONE notification of the right kind per transition,
// idempotent across ticks. Authoritative assertions are off the `billing_notifications`
// ledger (per-creator, per-kind), cross-fixture-safe exactly like the other kinds.
//
// RED pre-wiring: before the BillingNotificationKind::Spend* variants + the scan arm (g)
// existed, `scan_unsent` never read `spend_state_history`, so ZERO spend notifications
// were ever produced — `sent_count_kind(.., SpendWarn/Degrade/Block)` would all be 0.
// ===========================================================================

/// A process-wide gate serializing SPEND ticks across this binary's tests (the
/// spend-reconcile cron single-flights on its OWN fleet-wide advisory lock; a sibling
/// holding it would make my `tick` skip and win nothing). Mirrors `TICK_GATE`.
static SPEND_TICK_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_spend_gate() -> std::sync::MutexGuard<'static, ()> {
    SPEND_TICK_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Seed a plan charging 1 cent/request with `limit_cents` default spend cap, an app on
/// that plan, and an `owner` app_members row tying the app to `creator` (the H1 join the
/// notify scan resolves the creator through). Returns the app id.
async fn make_spend_app_owned_by(
    pg: &compio_postgres::Client,
    creator: Uuid,
    limit_cents: i64,
) -> (Uuid, String) {
    pg.execute(
        "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
         VALUES ('requests', 1, 1) \
         ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
        &[],
    )
    .await
    .expect("upsert requests weight");
    let plan_id = format!("pln_spnotf_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    pg.execute(
        "INSERT INTO zeroship.plans \
           (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
            runtime_limits_json, spend_limit_default_cents) \
         VALUES ($1, 'spend-notify-test', 0, 0, $3, \
                 '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', $2)",
        &[&plan_id, &limit_cents, &fx_one_cent],
    )
    .await
    .expect("seed priced plan");
    let app_name = format!("spend-notify-{}", Uuid::new_v4());
    let rows = pg
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&app_name, &plan_id, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app");
    let app_id: Uuid = rows[0].get("id");
    pg.execute(
        "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
        &[&app_id, &creator],
    )
    .await
    .expect("insert owner membership");
    (app_id, app_name)
}

/// Set this period's priced spend for `app` to exactly `cents` (1 cent/request ⇒ set the
/// `requests` usage_aggregates total). An authoritative DB write the spend cron reads —
/// lets the test position spend at any band boundary (including a DROP for the deadband
/// hold, which cumulative metering ingest could not express).
async fn set_spend_cents(pg: &compio_postgres::Client, app: Uuid, cents: i64) {
    // current period = date_trunc('month', now())::date — the same key evaluate_all uses.
    pg.execute(
        "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
         VALUES ($1, date_trunc('month', NOW())::date, 'requests', $2) \
         ON CONFLICT (app_id, period, metric) DO UPDATE SET total = EXCLUDED.total, updated_at = NOW()",
        &[&app, &cents],
    )
    .await
    .expect("set usage_aggregates total");
}

/// Set the per-app spend-limit override in `app_spend_limit` (the same config table
/// `SpendEngine::set_limit` writes). Raising the effective limit makes the next reconcile
/// tick see `limit_changed = true`, bypassing the anti-flap deadband so the app RECOVERS
/// one band in a single tick (a faithful downward/de-escalation edge).
async fn set_spend_limit_override(pg: &compio_postgres::Client, app: Uuid, cents: i64) {
    pg.execute(
        "INSERT INTO zeroship.app_spend_limit (app_id, spend_limit_cents, updated_at) \
         VALUES ($1, $2, NOW()) \
         ON CONFLICT (app_id) DO UPDATE SET spend_limit_cents = EXCLUDED.spend_limit_cents, \
           updated_at = NOW()",
        &[&app, &cents],
    )
    .await
    .expect("set app_spend_limit override");
}

/// Count spend_state_history transitions OUT OF / INTO a specific (from,to) edge for an app.
async fn spend_edge_count(
    pg: &compio_postgres::Client,
    app: Uuid,
    from_state: &str,
    to_state: &str,
) -> i64 {
    pg.query(
        "SELECT COUNT(*) AS c FROM zeroship.spend_state_history \
          WHERE app_id = $1 \
            AND from_state = $2::text::zeroship.spend_state \
            AND to_state   = $3::text::zeroship.spend_state",
        &[&app, &from_state, &to_state],
    )
    .await
    .expect("count spend edge")[0]
        .get::<_, i64>("c")
}

/// Read the current persisted spend state for an app (the cron's derived hot state).
async fn spend_state_of(pg: &compio_postgres::Client, app: Uuid) -> Option<String> {
    let rows = pg
        .query(
            "SELECT state::text AS state FROM zeroship.app_spend_state WHERE app_id = $1",
            &[&app],
        )
        .await
        .expect("read spend state");
    rows.first().map(|r| r.get::<_, String>("state"))
}

/// Count spend_state_history transitions INTO a given to_state for an app.
async fn spend_hist_count(pg: &compio_postgres::Client, app: Uuid, to_state: &str) -> i64 {
    pg.query(
        "SELECT COUNT(*) AS c FROM zeroship.spend_state_history \
          WHERE app_id = $1 AND to_state = $2::text::zeroship.spend_state",
        &[&app, &to_state],
    )
    .await
    .expect("count spend hist")[0]
        .get::<_, i64>("c")
}

/// Drive ONE gated spend-reconcile tick (serialized against sibling spend ticks).
async fn spend_tick(state: &AppState) {
    let _g = lock_spend_gate();
    spend_reconcile::tick(state).await.expect("spend tick");
}

#[compio::test]
async fn spend_band_walk_produces_one_notification_per_transition() {
    let Some(url) = db_url() else {
        eprintln!("SKIP spend_band_walk_produces_one_notification_per_transition: CONTROL_TEST_DB unset");
        return;
    };
    let fx = build_fixture(&url, "spend-band").await;
    let st = &*fx.state;

    // A creator (user + creator_billing) owning one app on a 100-cent-cap plan.
    let creator = make_creator(&fx.pg).await;
    let (app, app_name) = make_spend_app_owned_by(&fx.pg, creator, 100).await;

    // --- Walk the band: Allow→Warn (80%)→Degrade (95%)→Block (100%). Each tick is a REAL
    //     evaluate_all sweep; position spend at each boundary, tick, and confirm the
    //     persisted state advanced (so the spend_state_history row was genuinely written).
    set_spend_cents(&fx.pg, app, 80).await; // 80% ⇒ Warn
    spend_tick(st).await;
    assert_eq!(spend_state_of(&fx.pg, app).await.as_deref(), Some("warn"), "→warn");

    set_spend_cents(&fx.pg, app, 95).await; // 95% ⇒ Degrade
    spend_tick(st).await;
    assert_eq!(spend_state_of(&fx.pg, app).await.as_deref(), Some("degrade"), "→degrade");

    set_spend_cents(&fx.pg, app, 100).await; // 100% ⇒ Block
    spend_tick(st).await;
    assert_eq!(spend_state_of(&fx.pg, app).await.as_deref(), Some("block"), "→block");

    // --- Deadband HOLD: drop spend to 96% (≥ block_entry 100 − deadband 5 = 95) — a
    //     RELAXATION the hysteresis must HOLD at Block (limit_changed=false). No new
    //     transition row, so NO spend notification fires for the hold.
    set_spend_cents(&fx.pg, app, 96).await; // 96% < 100 but ≥ 95 ⇒ HOLD Block
    spend_tick(st).await;
    assert_eq!(
        spend_state_of(&fx.pg, app).await.as_deref(),
        Some("block"),
        "deadband holds Block (96% ≥ block_entry − deadband)"
    );

    // Exactly one transition row INTO each restrictive band (the hold added none).
    assert_eq!(spend_hist_count(&fx.pg, app, "warn").await, 1, "one →warn transition");
    assert_eq!(spend_hist_count(&fx.pg, app, "degrade").await, 1, "one →degrade transition");
    assert_eq!(spend_hist_count(&fx.pg, app, "block").await, 1, "one →block transition");

    // --- Now the notify cron. Drive it until MY creator's three spend transitions are all
    //     delivered (and zero pending), retrying past sibling lock contention.
    tick_until_sent(st, &fx.pg, &[(creator, 3)]).await;

    use BillingNotificationKind::{SpendBlock, SpendDegrade, SpendWarn};
    assert_eq!(
        sent_count_kind(&fx.pg, creator, SpendWarn).await,
        1,
        "exactly one spend_warn notification for the →warn transition"
    );
    assert_eq!(
        sent_count_kind(&fx.pg, creator, SpendDegrade).await,
        1,
        "exactly one spend_degrade notification for the →degrade transition"
    );
    assert_eq!(
        sent_count_kind(&fx.pg, creator, SpendBlock).await,
        1,
        "exactly one spend_block notification for the →block transition"
    );
    // The deadband hold produced no transition ⇒ no extra notification of any spend kind.
    let total_spend_sent = sent_count_kind(&fx.pg, creator, SpendWarn).await
        + sent_count_kind(&fx.pg, creator, SpendDegrade).await
        + sent_count_kind(&fx.pg, creator, SpendBlock).await;
    assert_eq!(total_spend_sent, 3, "exactly three spend notifications total (hold added none)");

    // --- Idempotent across ticks: a SECOND notify sweep produces nothing new for MY
    //     creator (every ledger row is `sent`, none `pending`).
    let pre = [
        (SpendWarn, sent_count_kind(&fx.pg, creator, SpendWarn).await),
        (SpendDegrade, sent_count_kind(&fx.pg, creator, SpendDegrade).await),
        (SpendBlock, sent_count_kind(&fx.pg, creator, SpendBlock).await),
    ];
    {
        let _gate = lock_tick_gate();
        billing_notify::tick(st).await.expect("second notify tick");
    }
    for (k, before) in pre {
        assert_eq!(
            sent_count_kind(&fx.pg, creator, k).await,
            before,
            "second sweep must not produce a new {k:?} sent row"
        );
    }
    assert_eq!(ledger_count(&fx.pg, creator, "pending").await, 0, "no pending rows remain");

    // The app NAME made it into the dedup transition_id mapping (sanity: the ledger rows
    // are keyed by the she_ transition id, one per band).
    let _ = app_name; // (name is asserted via the rendered body in notify.rs unit tests)
}

// ===========================================================================
// (HIGH, #10 follow-up) DOWNWARD / RECOVERY walk — escalation-only gate.
//
// `spend.rs::persist_transition` writes a `spend_state_history` row on EVERY edge,
// INCLUDING de-escalations (`block→degrade`, `degrade→warn`, `warn→allow`) once the
// effective limit rises and the deadband bypass relaxes the band. The notify scan arm (g)
// must email ONLY on ESCALATION (severity(to) > severity(from)). A recovery edge whose
// `to_state` is still warn/degrade/block must NOT email — telling a creator whose app is
// RECOVERING that it "is being throttled" / "approaching the limit" is wrong.
//
// RED proof: the original arm (g) filtered solely on `to_state IN ('warn','degrade','block')`
// with NO direction check, so the `block→degrade` edge produced a spurious SpendDegrade
// ("being throttled") email and `degrade→warn` produced a spurious SpendWarn ("approaching
// your limit") email on the way DOWN. This test walks the band DOWN (Block→Degrade→Warn→
// Allow) via real limit-raise reconcile ticks and asserts ZERO spend notifications — it
// FAILS against the pre-fix code (2 spurious recovery emails), passes after the SQL gate.
// ===========================================================================
#[compio::test]
async fn spend_band_recovery_walk_sends_no_notifications() {
    let Some(url) = db_url() else {
        eprintln!("SKIP spend_band_recovery_walk_sends_no_notifications: CONTROL_TEST_DB unset");
        return;
    };
    let fx = build_fixture(&url, "spend-recovery").await;
    let st = &*fx.state;

    let creator = make_creator(&fx.pg).await;
    let (app, _app_name) = make_spend_app_owned_by(&fx.pg, creator, 100).await;

    use BillingNotificationKind::{SpendBlock, SpendDegrade, SpendWarn};

    // --- Climb to Block first (spend = limit = 100 ⇒ 100% ⇒ Block). One upward tick.
    //     This `allow→block` IS a legitimate escalation and DOES email — we drain it and
    //     snapshot the baseline so the recovery walk below is measured as a DELTA (it must
    //     add zero).
    set_spend_cents(&fx.pg, app, 100).await;
    spend_tick(st).await;
    assert_eq!(spend_state_of(&fx.pg, app).await.as_deref(), Some("block"), "→block");
    tick_until_sent(st, &fx.pg, &[(creator, 1)]).await; // the single allow→block escalation
    let base_warn = sent_count_kind(&fx.pg, creator, SpendWarn).await;
    let base_degrade = sent_count_kind(&fx.pg, creator, SpendDegrade).await;
    let base_block = sent_count_kind(&fx.pg, creator, SpendBlock).await;
    assert_eq!(
        (base_warn, base_degrade, base_block),
        (0, 0, 1),
        "the upward allow→block escalation sent exactly one spend_block (and nothing else)"
    );

    // --- Now walk DOWN one band per tick by RAISING the effective limit (override). Each
    //     raise makes `limit_changed = true`, bypassing the deadband so the band relaxes
    //     immediately to the new raw band. Spend stays pinned at 100 cents throughout.
    //
    //   limit 105 ⇒ pct = 100*100/105 = 95 ⇒ raw Degrade ⇒ edge block→degrade
    set_spend_limit_override(&fx.pg, app, 105).await;
    spend_tick(st).await;
    assert_eq!(spend_state_of(&fx.pg, app).await.as_deref(), Some("degrade"), "block→degrade");

    //   limit 120 ⇒ pct = 100*100/120 = 83 ⇒ raw Warn ⇒ edge degrade→warn
    set_spend_limit_override(&fx.pg, app, 120).await;
    spend_tick(st).await;
    assert_eq!(spend_state_of(&fx.pg, app).await.as_deref(), Some("warn"), "degrade→warn");

    //   limit 200 ⇒ pct = 100*100/200 = 50 ⇒ raw Allow ⇒ edge warn→allow
    set_spend_limit_override(&fx.pg, app, 200).await;
    spend_tick(st).await;
    assert_eq!(spend_state_of(&fx.pg, app).await.as_deref(), Some("allow"), "warn→allow");

    // The three downward edges were genuinely written (history rows exist with the
    // recovering `from`/`to` endpoints) — so the notify scan really has rows to (not) email.
    assert_eq!(spend_edge_count(&fx.pg, app, "block", "degrade").await, 1, "block→degrade row");
    assert_eq!(spend_edge_count(&fx.pg, app, "degrade", "warn").await, 1, "degrade→warn row");
    assert_eq!(spend_edge_count(&fx.pg, app, "warn", "allow").await, 1, "warn→allow row");

    // --- Run the notify cron to convergence. NONE of the recovery edges may email.
    {
        let _gate = lock_tick_gate();
        billing_notify::tick(st).await.expect("notify tick");
        // A second sweep to be sure nothing was left pending to re-drive into a send.
        billing_notify::tick(st).await.expect("second notify tick");
    }

    let warn = sent_count_kind(&fx.pg, creator, SpendWarn).await;
    let degrade = sent_count_kind(&fx.pg, creator, SpendDegrade).await;
    let block = sent_count_kind(&fx.pg, creator, SpendBlock).await;
    // DELTA vs the baseline (the upward allow→block): the recovery walk must add NOTHING.
    // Pre-fix, block→degrade adds a SpendDegrade and degrade→warn adds a SpendWarn.
    assert_eq!(warn, base_warn, "NO new spend_warn on the degrade→warn recovery edge");
    assert_eq!(degrade, base_degrade, "NO new spend_degrade on the block→degrade recovery edge");
    assert_eq!(block, base_block, "NO new spend_block on any recovery edge");
    assert_eq!(
        (warn + degrade + block) - (base_warn + base_degrade + base_block),
        0,
        "a RECOVERING app must receive ZERO additional spend escalation notifications"
    );
    // And no half-claimed pending rows linger either (claim-before-send would have inserted
    // one per spurious candidate; the gate must drop them before the claim).
    assert_eq!(
        ledger_count(&fx.pg, creator, "pending").await,
        0,
        "no pending spend notification rows for a recovery walk"
    );
}

// ===========================================================================
// (#6 watermark) a creator_billing_status_history transition aged past the
// 30-day NOTIFY_SCAN_WINDOW is NOT picked up by the notify scan — the watermark
// caps the sweep so long-dead transitions are abandoned, never belatedly emailed.
// ===========================================================================

/// Age a creator's `creator_billing_status_history` rows back `days` so the source
/// transition falls outside the notify scan window (the `h.at > NOW() - 30 days` bound).
async fn age_history(pg: &compio_postgres::Client, creator: Uuid, days: i64) {
    pg.execute(
        "UPDATE zeroship.creator_billing_status_history \
            SET at = NOW() - make_interval(days => $2::int) WHERE creator_id = $1",
        &[&creator, &(days as i32)],
    )
    .await
    .expect("age history");
}

#[compio::test]
async fn aged_transition_past_scan_window_is_not_notified() {
    let Some(url) = db_url() else {
        eprintln!("SKIP aged_transition_past_scan_window_is_not_notified: CONTROL_TEST_DB unset");
        return;
    };
    let fx = build_fixture(&url, "watermark").await;
    let st = &*fx.state;
    let _gate = lock_tick_gate(); // own the sweep so a sibling can't claim my aged row

    // A creator with ONE past_due transition (a cbh_ history row).
    let creator = make_creator(&fx.pg).await;
    fail_payment(st, creator, 1_000).await;
    // Age the transition WELL past the 30-day NOTIFY_SCAN_WINDOW.
    age_history(&fx.pg, creator, 45).await;

    // Sweep a few times: the aged transition must NEVER be scanned/claimed/sent.
    for _ in 0..3 {
        billing_notify::sweep(st).await.expect("sweep");
    }
    assert_eq!(
        sent_count_kind(&fx.pg, creator, BillingNotificationKind::PastDue).await,
        0,
        "a transition aged past the 30-day scan window is abandoned — never notified",
    );
    assert_eq!(
        ledger_count(&fx.pg, creator, "pending").await,
        0,
        "the aged transition is never even claimed (no pending ledger row)",
    );

    // Control: a FRESH transition for the same creator IS picked up — proving the
    // sweep is working and the watermark (not some other reason) excluded the aged row.
    fail_payment(st, creator, 2_000).await; // a new past_due edge is a no-op state-wise,
    // but to get a fresh actionable edge, drive a recovery then a re-failure.
    recover_payment(st, creator, 3_000).await; // past_due→active (a fresh `recovered` cbh row, at=NOW)
    let mut settled = false;
    for _ in 0..50 {
        billing_notify::sweep(st).await.expect("sweep fresh");
        if sent_count_kind(&fx.pg, creator, BillingNotificationKind::Recovered).await >= 1 {
            settled = true;
            break;
        }
    }
    assert!(settled, "a FRESH (in-window) transition IS notified — the sweep works");
    // The aged past_due STILL never fired.
    assert_eq!(
        sent_count_kind(&fx.pg, creator, BillingNotificationKind::PastDue).await,
        0,
        "the aged past_due remains un-notified even after the sweep delivered a fresh row",
    );
}

// ===========================================================================
// (#8) dunning tick's advisory-lock single-flight: a second concurrent tick that
// cannot acquire the dunning advisory lock no-ops (mirrors the spend-reconcile and
// notify lock tests). The dunning key is distinct from spend/notify so the sweeps
// never block each other.
// ===========================================================================
#[compio::test]
async fn dunning_tick_skips_when_advisory_lock_held() {
    let Some(url) = db_url() else {
        eprintln!("SKIP dunning_tick_skips_when_advisory_lock_held: CONTROL_TEST_DB unset");
        return;
    };
    let fx = build_fixture(&url, "dunning-lock").await;
    let st = &*fx.state;

    // A creator past the dunning window — `suspend_exhausted` WOULD suspend it if the
    // sweep ran. (Drive a real past_due, then backdate the dunning clock.)
    let creator = make_creator(&fx.pg).await;
    fail_payment(st, creator, 1_000).await;
    fx.pg
        .execute(
            "UPDATE zeroship.creator_billing_status \
                SET past_due_since = NOW() - make_interval(days => $2::int) WHERE creator_id = $1",
            &[&creator, &((DEFAULT_MAX_DUNNING_DAYS + 1) as i32)],
        )
        .await
        .expect("backdate past_due");

    // Hold the DUNNING advisory lock on a side session (the cron's key, "zsdunn").
    const DUNNING_LOCK_KEY: i64 = 0x7a73_6475_6e6e_0001;
    let (side, side_conn) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("side lock conn");
    compio::runtime::spawn(async move {
        let _ = side_conn.run().await;
    })
    .detach();
    let got: bool = side
        .query("SELECT pg_try_advisory_lock($1) AS locked", &[&DUNNING_LOCK_KEY])
        .await
        .expect("hold dunning lock")[0]
        .get("locked");
    assert!(got, "side conn acquires the dunning lock");

    // A tick that loses the lock must NO-OP: zero suspensions, creator stays past_due.
    let n = dunning::tick(st, DEFAULT_MAX_DUNNING_DAYS).await.expect("tick while locked");
    assert_eq!(n, 0, "a dunning tick that loses the advisory lock suspends NOBODY");
    let state = db_state_of(&fx.pg, creator).await;
    assert_eq!(
        state.as_deref(),
        Some("past_due"),
        "the exhausted creator is NOT suspended while the lock is held elsewhere",
    );

    // Release the lock; now a tick proceeds and suspends the exhausted creator.
    side.execute("SELECT pg_advisory_unlock($1)", &[&DUNNING_LOCK_KEY])
        .await
        .expect("release dunning lock");
    let n2 = dunning::tick(st, DEFAULT_MAX_DUNNING_DAYS).await.expect("tick runs");
    assert!(n2 >= 1, "after release, the sweep runs and suspends our exhausted creator");
    assert_eq!(
        db_state_of(&fx.pg, creator).await.as_deref(),
        Some("suspended"),
        "the exhausted creator is suspended once the lock is free",
    );
}

/// Read the persisted account state for a creator (None ⇒ no row).
async fn db_state_of(pg: &compio_postgres::Client, creator: Uuid) -> Option<String> {
    pg.query(
        "SELECT state FROM zeroship.creator_billing_status WHERE creator_id = $1",
        &[&creator],
    )
    .await
    .expect("read state")
    .first()
    .map(|r| r.get::<_, String>("state"))
}
