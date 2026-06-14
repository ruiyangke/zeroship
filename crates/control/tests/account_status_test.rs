//! Faithful PG integration tests for the billing G2 account-status state machine
//! (`crates/control/src/account_status.rs`) — the payment-failure → dunning →
//! suspension lifecycle that the gateway gates dispatch on.
//!
//! These run the REAL `AccountStatusStore` methods + the dunning `suspend_exhausted`
//! sweep against a LIVE Postgres (no shims): `record_payment_failed`,
//! `record_payment_recovered`, `suspend_exhausted` (the `make_interval` window
//! comparison), and the order-safety guard (`last_recovered_at` high-water).
//!
//! The spec mandates four transitions plus the out-of-order regression:
//!   * `payment_failed_moves_to_past_due`
//!   * `repeated_failure_is_idempotent`
//!   * `dunning_exhaustion_suspends`
//!   * `payment_success_reactivates`        (suspended → active, reversible)
//!   * `out_of_order_paid_then_failed_does_not_resuspend`   (critic #1 regression)
//!
//! Gated on `CONTROL_TEST_DB`; silent skip otherwise. The DB must have changeset
//! 0045 applied.

use uuid::Uuid;

use zeroship_control::account_status::{AccountStatusStore, DEFAULT_MAX_DUNNING_DAYS};
use zeroship_control::Registry;
use zeroship_core::types::AccountState;

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// A live registry + a side connection for seeding users and reading raw columns.
struct Fx {
    registry: Registry,
    pg: compio_postgres::Client,
}

async fn fx() -> Fx {
    let url = db_url().expect("CONTROL_TEST_DB checked by caller");
    let registry = Registry::new(&url).await.expect("registry");
    let (pg, conn) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("side connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    Fx { registry, pg }
}

/// Insert a fresh `zeroship.users` row (the FK target of `creator_billing_status`)
/// and return its id. Unique email per call so tests never collide.
async fn make_creator(pg: &compio_postgres::Client) -> Uuid {
    let rows = pg
        .query(
            "INSERT INTO zeroship.users (email, name) \
             VALUES ($1, 'acct-status-test') RETURNING id",
            &[&format!("acct-{}@test.invalid", Uuid::new_v4().simple())],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

/// Read the persisted state TEXT for a creator (None ⇒ no row).
async fn db_state(pg: &compio_postgres::Client, creator: Uuid) -> Option<String> {
    pg.query(
        "SELECT state FROM zeroship.creator_billing_status WHERE creator_id = $1",
        &[&creator],
    )
    .await
    .expect("read state")
    .first()
    .map(|r| r.get::<_, String>("state"))
}

/// Force the dunning clock back `days` so the window is exhausted without sleeping.
async fn backdate_past_due(pg: &compio_postgres::Client, creator: Uuid, days: i64) {
    pg.execute(
        "UPDATE zeroship.creator_billing_status \
         SET past_due_since = NOW() - make_interval(days => $2::int) \
         WHERE creator_id = $1",
        &[&creator, &(days as i32)],
    )
    .await
    .expect("backdate past_due_since");
}

const T0: i64 = 1_777_000_000; // a fixed Stripe-style `event.created` baseline.

#[compio::test]
async fn payment_failed_moves_to_past_due() {
    let Some(_) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());
    let creator = make_creator(&f.pg).await;

    // active (no row) → past_due.
    let t = store
        .record_payment_failed(creator, Some("in_first"), T0)
        .await
        .expect("record failure")
        .expect("a transition occurred");
    assert_eq!(t.from, AccountState::Active);
    assert_eq!(t.to, AccountState::PastDue);
    assert_eq!(t.reason, "payment_failed");

    assert_eq!(db_state(&f.pg, creator).await.as_deref(), Some("past_due"));
    assert_eq!(
        store.get_state(creator).await.expect("get_state"),
        Some(AccountState::PastDue)
    );

    // One history row for the edge.
    let hist = f
        .pg
        .query(
            "SELECT from_state, to_state, reason \
             FROM zeroship.creator_billing_status_history WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("read history");
    assert_eq!(hist.len(), 1, "exactly one transition recorded");
    assert_eq!(hist[0].get::<_, String>("to_state"), "past_due");
}

#[compio::test]
async fn repeated_failure_is_idempotent() {
    let Some(_) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());
    let creator = make_creator(&f.pg).await;

    // First failure arms the window.
    store
        .record_payment_failed(creator, Some("in_a"), T0)
        .await
        .expect("first failure")
        .expect("active→past_due");
    let since_after_first: chrono::DateTime<chrono::Utc> = f
        .pg
        .query(
            "SELECT past_due_since FROM zeroship.creator_billing_status WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .unwrap()[0]
        .get("past_due_since");

    // A redelivered / subsequent failure (later event.created) must NOT restart
    // the clock and must NOT report a transition (already past_due).
    let again = store
        .record_payment_failed(creator, Some("in_a"), T0 + 100)
        .await
        .expect("repeat failure");
    assert!(again.is_none(), "no state change on a repeat failure");

    let since_after_second: chrono::DateTime<chrono::Utc> = f
        .pg
        .query(
            "SELECT past_due_since FROM zeroship.creator_billing_status WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .unwrap()[0]
        .get("past_due_since");
    assert_eq!(
        since_after_first, since_after_second,
        "the dunning clock is NOT restarted by a repeat failure (idempotent window)"
    );

    // Still exactly one history row.
    let n: i64 = f
        .pg
        .query(
            "SELECT COUNT(*) AS n FROM zeroship.creator_billing_status_history WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .unwrap()[0]
        .get("n");
    assert_eq!(n, 1, "no extra history row for an idempotent repeat");
}

#[compio::test]
async fn dunning_exhaustion_suspends() {
    let Some(_) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());

    // A past_due creator inside the window is NOT suspended; one past the window IS.
    let fresh = make_creator(&f.pg).await;
    store
        .record_payment_failed(fresh, Some("in_fresh"), T0)
        .await
        .expect("fresh failure");

    let stale = make_creator(&f.pg).await;
    store
        .record_payment_failed(stale, Some("in_stale"), T0)
        .await
        .expect("stale failure");
    // Push the stale creator's window past max_dunning_days.
    backdate_past_due(&f.pg, stale, DEFAULT_MAX_DUNNING_DAYS + 1).await;

    let transitions = store
        .suspend_exhausted(DEFAULT_MAX_DUNNING_DAYS)
        .await
        .expect("dunning sweep");

    // The stale creator (and ONLY it among ours) is suspended.
    assert!(
        transitions.iter().any(|t| t.creator_id == stale
            && t.from == AccountState::PastDue
            && t.to == AccountState::Suspended
            && t.reason == "dunning_exhausted"),
        "the exhausted-window creator is suspended"
    );
    assert!(
        !transitions.iter().any(|t| t.creator_id == fresh),
        "the in-window creator is NOT suspended"
    );

    assert_eq!(db_state(&f.pg, stale).await.as_deref(), Some("suspended"));
    assert_eq!(db_state(&f.pg, fresh).await.as_deref(), Some("past_due"));

    // suspended_at stamped.
    let suspended_at: Option<chrono::DateTime<chrono::Utc>> = f
        .pg
        .query(
            "SELECT suspended_at FROM zeroship.creator_billing_status WHERE creator_id = $1",
            &[&stale],
        )
        .await
        .unwrap()[0]
        .get("suspended_at");
    assert!(suspended_at.is_some(), "suspended_at is stamped on suspension");
}

#[compio::test]
async fn payment_success_reactivates() {
    let Some(_) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());
    let creator = make_creator(&f.pg).await;

    // Drive all the way to suspended: fail, exhaust window, sweep.
    store
        .record_payment_failed(creator, Some("in_x"), T0)
        .await
        .expect("failure");
    backdate_past_due(&f.pg, creator, DEFAULT_MAX_DUNNING_DAYS + 1).await;
    store
        .suspend_exhausted(DEFAULT_MAX_DUNNING_DAYS)
        .await
        .expect("sweep");
    assert_eq!(db_state(&f.pg, creator).await.as_deref(), Some("suspended"));

    // Recovery (invoice.paid) un-suspends straight back to active (REVERSIBILITY).
    let t = store
        .record_payment_recovered(creator, T0 + 1_000)
        .await
        .expect("recover")
        .expect("suspended→active transition");
    assert_eq!(t.from, AccountState::Suspended);
    assert_eq!(t.to, AccountState::Active);
    assert_eq!(t.reason, "payment_recovered");

    assert_eq!(db_state(&f.pg, creator).await.as_deref(), Some("active"));
    // Window + suspension cleared.
    let row = &f
        .pg
        .query(
            "SELECT past_due_since, suspended_at, failed_invoice_id \
             FROM zeroship.creator_billing_status WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .unwrap()[0];
    assert!(
        row.get::<_, Option<chrono::DateTime<chrono::Utc>>>("past_due_since").is_none(),
        "past_due_since cleared on recovery"
    );
    assert!(
        row.get::<_, Option<chrono::DateTime<chrono::Utc>>>("suspended_at").is_none(),
        "suspended_at cleared on recovery"
    );

    // A second recovery is a no-op (already active).
    let again = store
        .record_payment_recovered(creator, T0 + 2_000)
        .await
        .expect("idempotent recover");
    assert!(again.is_none(), "recovery of an already-active creator is a no-op");
}

/// MAJOR #5 regression — `registry.get_routes` must be DETERMINISTIC under an
/// owner fan-out (a data-integrity multi-`role='owner'` situation), and on a
/// fan-out must surface the MOST-RESTRICTIVE account_state (suspended > past_due
/// > active) so a fan-out can never un-suspend a suspended creator.
///
/// We seed ONE app with TWO owner rows: one creator suspended, one active. The
/// LATERAL `DISTINCT ON (app_id)` + restrictiveness ORDER BY must always pick
/// `suspended` regardless of which owner row the planner would otherwise pick.
#[compio::test]
async fn get_routes_fanout_picks_most_restrictive_account_state() {
    let Some(_) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());

    // Two creators: one we'll suspend, one stays active.
    let suspended = make_creator(&f.pg).await;
    let active = make_creator(&f.pg).await;

    // Drive `suspended` to suspended.
    store
        .record_payment_failed(suspended, Some("in_s"), T0)
        .await
        .expect("failure");
    backdate_past_due(&f.pg, suspended, DEFAULT_MAX_DUNNING_DAYS + 1).await;
    store
        .suspend_exhausted(DEFAULT_MAX_DUNNING_DAYS)
        .await
        .expect("sweep");
    assert_eq!(db_state(&f.pg, suspended).await.as_deref(), Some("suspended"));

    // `apps.plan_id` has an FK to `plans` — seed the built-in plans and pick one
    // real plan id to satisfy it.
    zeroship_control::bootstrap_console::seed_plans(&f.registry)
        .await
        .expect("seed built-in plans");
    let plan_id: String = f
        .pg
        .query("SELECT id FROM zeroship.plans LIMIT 1", &[])
        .await
        .expect("read a plan")[0]
        .get("id");

    // One app with BOTH creators as owner rows (the fan-out).
    let app_rows = f
        .pg
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[
                &format!("fanout-{}", Uuid::new_v4().simple()),
                &plan_id,
                &Uuid::new_v4().to_string(),
            ],
        )
        .await
        .expect("insert app");
    let app_id: Uuid = app_rows[0].get("id");
    for owner in [active, suspended] {
        f.pg
            .execute(
                "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
                &[&app_id, &owner],
            )
            .await
            .expect("insert owner");
    }

    // get_routes must surface the MOST-RESTRICTIVE state (suspended), not a
    // last-write-wins coin flip. Run several times to defeat planner luck.
    for _ in 0..5 {
        let routes = f.registry.get_routes().await.expect("get_routes");
        let entry = routes.get(&app_id).expect("our app is present");
        assert_eq!(
            entry.account_state,
            AccountState::Suspended,
            "a fan-out must resolve to the most-restrictive account_state (suspended), \
             never un-suspend via last-write-wins"
        );
    }
}

/// CRITICAL #1 regression — an out-of-order/redelivered `payment_failed` whose
/// Stripe `event.created` predates a recovery MUST NOT re-arm `past_due` on an
/// already-recovered (paying) creator, and the dunning sweep must therefore NOT
/// suspend them.
///
/// RED before the fix: `record_payment_failed` guarded only on `state`, so a
/// stale failure delivered AFTER recovery would flip active→past_due and the
/// sweep would suspend a paying creator. GREEN: the `last_recovered_at`
/// high-water (stamped from `event.created`) makes the stale failure a no-op.
#[compio::test]
async fn out_of_order_paid_then_failed_does_not_resuspend() {
    let Some(_) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());
    let creator = make_creator(&f.pg).await;

    // A real failure arms past_due at an EARLY event time.
    let early = T0;
    store
        .record_payment_failed(creator, Some("in_dunning"), early)
        .await
        .expect("failure")
        .expect("active→past_due");

    // Recovery (invoice.paid) lands with a LATER event.created → back to active,
    // stamps last_recovered_at = recover_at.
    let recover_at = T0 + 500;
    store
        .record_payment_recovered(creator, recover_at)
        .await
        .expect("recover")
        .expect("past_due→active");
    assert_eq!(db_state(&f.pg, creator).await.as_deref(), Some("active"));

    // Now a STALE/redelivered `payment_failed` for the SAME invoice arrives, but
    // its event.created PREDATES the recovery (early < recover_at). The
    // order-safety guard must IGNORE it: no transition, stays active, NOT re-armed.
    let stale = store
        .record_payment_failed(creator, Some("in_dunning"), early)
        .await
        .expect("stale failure handled");
    assert!(
        stale.is_none(),
        "a failure predating the recovery must NOT produce a transition"
    );
    assert_eq!(
        db_state(&f.pg, creator).await.as_deref(),
        Some("active"),
        "the stale failure must NOT re-arm past_due on a recovered creator"
    );
    // past_due_since must still be NULL (window not re-armed).
    let since: Option<chrono::DateTime<chrono::Utc>> = f
        .pg
        .query(
            "SELECT past_due_since FROM zeroship.creator_billing_status WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .unwrap()[0]
        .get("past_due_since");
    assert!(since.is_none(), "dunning window must not be re-armed by a stale failure");

    // And the dunning sweep must NOT suspend this paying creator even if a
    // (hypothetically) old window existed — the guard kept state active.
    let transitions = store
        .suspend_exhausted(DEFAULT_MAX_DUNNING_DAYS)
        .await
        .expect("sweep");
    assert!(
        !transitions.iter().any(|t| t.creator_id == creator),
        "the dunning cron must NOT suspend a recovered creator hit by a stale failure"
    );
    assert_eq!(db_state(&f.pg, creator).await.as_deref(), Some("active"));

    // A FRESH failure (event.created AFTER the recovery) DOES legitimately re-arm
    // — the guard only blocks STALE events, not genuine post-recovery failures.
    let fresh_fail_at = recover_at + 100;
    let re = store
        .record_payment_failed(creator, Some("in_new"), fresh_fail_at)
        .await
        .expect("fresh failure")
        .expect("active→past_due on a genuine post-recovery failure");
    assert_eq!(re.to, AccountState::PastDue);
    assert_eq!(db_state(&f.pg, creator).await.as_deref(), Some("past_due"));
}
