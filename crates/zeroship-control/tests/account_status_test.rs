//! Faithful PG integration tests for the billing G2 account-status state machine
//! (`crates/zeroship-control/src/account_status.rs`) — the payment-failure → dunning →
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
//! Gated on a configured test database
//! (`common::require_control_db`); an absent or unmigrated one REFUSES
//! the run.
//! The DB must have changeset 0045 applied.

use crate::common;

use uuid::Uuid;

use zeroship_control::account_status::{AccountStatusStore, DEFAULT_MAX_DUNNING_DAYS};
use zeroship_control::Registry;
use zeroship_core::types::AccountState;

fn db_url() -> String {
    common::require_control_db()
}

/// A live registry + a side connection for seeding users and reading raw columns.
struct Fx {
    registry: Registry,
    pg: compio_postgres::Client,
}

async fn fx() -> Fx {
    let url = db_url();
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

/// A fresh `zeroship.organizations` row: the subject
/// `organization_billing_status` keys on and the FK target its parent
/// `organization_billing` points at. Unique slug/email per call so tests never
/// collide.
async fn make_organization(pg: &compio_postgres::Client) -> String {
    let organization_id = zeroship_core::typed_id::generate("org");
    let slug = format!("acct-{}", Uuid::new_v4().simple());
    pg.execute(
        "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
         VALUES ($1, $2, 'acct-status-test', $3)",
        &[&organization_id, &slug, &format!("{slug}@test.invalid")],
    )
    .await
    .expect("insert organization");
    organization_id
}

/// Read the persisted state TEXT for a organization (None ⇒ no row).
async fn db_state(pg: &compio_postgres::Client, organization: &str) -> Option<String> {
    pg.query(
        "SELECT state FROM zeroship.organization_billing_status WHERE organization_id = $1",
        &[&organization],
    )
    .await
    .expect("read state")
    .first()
    .map(|r| r.get::<_, String>("state"))
}

/// Force the dunning clock back `days` so the window is exhausted without sleeping.
async fn backdate_past_due(pg: &compio_postgres::Client, organization: &str, days: i64) {
    pg.execute(
        "UPDATE zeroship.organization_billing_status \
         SET past_due_since = NOW() - make_interval(days => $2::int) \
         WHERE organization_id = $1",
        &[&organization, &(days as i32)],
    )
    .await
    .expect("backdate past_due_since");
}

const T0: i64 = 1_777_000_000; // a fixed Stripe-style `event.created` baseline.

#[compio::test]
async fn payment_failed_moves_to_past_due() {
    let _ = db_url();
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());
    let organization = make_organization(&f.pg).await;
    let organization = organization.as_str();

    // active (no row) → past_due.
    let t = store
        .record_payment_failed(organization, Some("in_first"), T0)
        .await
        .expect("record failure")
        .expect("a transition occurred");
    assert_eq!(t.from, AccountState::Active);
    assert_eq!(t.to, AccountState::PastDue);
    assert_eq!(t.reason, "payment_failed");

    assert_eq!(db_state(&f.pg, organization).await.as_deref(), Some("past_due"));
    assert_eq!(
        store.get_state(organization).await.expect("get_state"),
        Some(AccountState::PastDue)
    );

    // One history row for the edge.
    let hist = f
        .pg
        .query(
            "SELECT from_state, to_state, reason \
             FROM zeroship.organization_billing_status_history WHERE organization_id = $1",
            &[&organization],
        )
        .await
        .expect("read history");
    assert_eq!(hist.len(), 1, "exactly one transition recorded");
    assert_eq!(hist[0].get::<_, String>("to_state"), "past_due");

    // Teardown: `store` and `f` (its registry + a raw side connection) each
    // hold a connection, and locals are dropped only after the body returns -
    // by which point the runtime is gone and the sockets can no longer be
    // closed. Drop them explicitly, then wait for the close to land.
    drop(store);
    drop(f);
    common::drain_pg().await;
}

#[compio::test]
async fn repeated_failure_is_idempotent() {
    let _ = db_url();
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());
    let organization = make_organization(&f.pg).await;
    let organization = organization.as_str();

    // First failure arms the window.
    store
        .record_payment_failed(organization, Some("in_a"), T0)
        .await
        .expect("first failure")
        .expect("active→past_due");
    let since_after_first: chrono::DateTime<chrono::Utc> = f
        .pg
        .query(
            "SELECT past_due_since FROM zeroship.organization_billing_status WHERE organization_id = $1",
            &[&organization],
        )
        .await
        .unwrap()[0]
        .get("past_due_since");

    // A redelivered / subsequent failure (later event.created) must NOT restart
    // the clock and must NOT report a transition (already past_due).
    let again = store
        .record_payment_failed(organization, Some("in_a"), T0 + 100)
        .await
        .expect("repeat failure");
    assert!(again.is_none(), "no state change on a repeat failure");

    let since_after_second: chrono::DateTime<chrono::Utc> = f
        .pg
        .query(
            "SELECT past_due_since FROM zeroship.organization_billing_status WHERE organization_id = $1",
            &[&organization],
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
            "SELECT COUNT(*) AS n FROM zeroship.organization_billing_status_history WHERE organization_id = $1",
            &[&organization],
        )
        .await
        .unwrap()[0]
        .get("n");
    assert_eq!(n, 1, "no extra history row for an idempotent repeat");

    drop(store);
    drop(f);
    common::drain_pg().await;
}

#[compio::test]
async fn dunning_exhaustion_suspends() {
    let _ = db_url();
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());

    // A past_due organization inside the window is NOT suspended; one past the window IS.
    let fresh = make_organization(&f.pg).await;
    let fresh = fresh.as_str();
    store
        .record_payment_failed(fresh, Some("in_fresh"), T0)
        .await
        .expect("fresh failure");

    let stale = make_organization(&f.pg).await;
    let stale = stale.as_str();
    store
        .record_payment_failed(stale, Some("in_stale"), T0)
        .await
        .expect("stale failure");
    // Push the stale organization's window past max_dunning_days.
    backdate_past_due(&f.pg, stale, DEFAULT_MAX_DUNNING_DAYS + 1).await;

    let transitions = store
        .suspend_exhausted(DEFAULT_MAX_DUNNING_DAYS)
        .await
        .expect("dunning sweep");

    // The stale organization (and ONLY it among ours) is suspended.
    assert!(
        transitions.iter().any(|t| t.organization_id == stale
            && t.from == AccountState::PastDue
            && t.to == AccountState::Suspended
            && t.reason == "dunning_exhausted"),
        "the exhausted-window organization is suspended"
    );
    assert!(
        !transitions.iter().any(|t| t.organization_id == fresh),
        "the in-window organization is NOT suspended"
    );

    assert_eq!(db_state(&f.pg, stale).await.as_deref(), Some("suspended"));
    assert_eq!(db_state(&f.pg, fresh).await.as_deref(), Some("past_due"));

    // suspended_at stamped.
    let suspended_at: Option<chrono::DateTime<chrono::Utc>> = f
        .pg
        .query(
            "SELECT suspended_at FROM zeroship.organization_billing_status WHERE organization_id = $1",
            &[&stale],
        )
        .await
        .unwrap()[0]
        .get("suspended_at");
    assert!(suspended_at.is_some(), "suspended_at is stamped on suspension");

    drop(store);
    drop(f);
    common::drain_pg().await;
}

#[compio::test]
async fn payment_success_reactivates() {
    let _ = db_url();
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());
    let organization = make_organization(&f.pg).await;
    let organization = organization.as_str();

    // Drive all the way to suspended: fail, exhaust window, sweep.
    store
        .record_payment_failed(organization, Some("in_x"), T0)
        .await
        .expect("failure");
    backdate_past_due(&f.pg, organization, DEFAULT_MAX_DUNNING_DAYS + 1).await;
    store
        .suspend_exhausted(DEFAULT_MAX_DUNNING_DAYS)
        .await
        .expect("sweep");
    assert_eq!(db_state(&f.pg, organization).await.as_deref(), Some("suspended"));

    // Recovery (invoice.paid) un-suspends straight back to active (REVERSIBILITY).
    let t = store
        .record_payment_recovered(organization, T0 + 1_000)
        .await
        .expect("recover")
        .expect("suspended→active transition");
    assert_eq!(t.from, AccountState::Suspended);
    assert_eq!(t.to, AccountState::Active);
    assert_eq!(t.reason, "payment_recovered");

    assert_eq!(db_state(&f.pg, organization).await.as_deref(), Some("active"));
    // Window + suspension cleared.
    let row = &f
        .pg
        .query(
            "SELECT past_due_since, suspended_at, failed_invoice_id \
             FROM zeroship.organization_billing_status WHERE organization_id = $1",
            &[&organization],
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
        .record_payment_recovered(organization, T0 + 2_000)
        .await
        .expect("idempotent recover");
    assert!(again.is_none(), "recovery of an already-active organization is a no-op");

    drop(store);
    drop(f);
    common::drain_pg().await;
}

/// `registry.get_routes` must report EACH app the account state of ITS OWN
/// organization, and no other.
///
/// This replaced a fan-out determinism test, and the replacement is narrower
/// because the hazard is. While the billing subject was a human, one app
/// reached several `organization_billing_status` rows through its
/// organization's owners, so the join needed a LIMIT 1 ordered
/// most-restrictive-first to stop a second owner with a good card from
/// un-suspending everyone. `apps.organization_id` is single-valued and the
/// status table keys on it, so no app can reach two states and there is nothing
/// to order.
///
/// What CAN still go wrong is the join naming the wrong column, which would
/// leak one organization's enforcement onto another's apps or drop it
/// entirely. Two organizations, one suspended and one not, with an app each,
/// bind exactly that: the assertion fails in both directions.
#[compio::test]
async fn get_routes_reports_each_app_its_own_organization_state() {
    let _ = db_url();
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());

    let suspended = make_organization(&f.pg).await;
    let suspended = suspended.as_str();
    let active = make_organization(&f.pg).await;
    let active = active.as_str();

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
    zeroship_control::plan_catalog::seed_plans(&f.registry)
        .await
        .expect("seed built-in plans");
    let plan_id: String = f
        .pg
        .query("SELECT id FROM zeroship.plans LIMIT 1", &[])
        .await
        .expect("read a plan")[0]
        .get("id");

    let suspended_app = common::seed_app_in_organization(
        &f.pg,
        &format!("suspended-{}", Uuid::new_v4().simple()),
        &plan_id,
        suspended,
    )
    .await;
    let active_app = common::seed_app_in_organization(
        &f.pg,
        &format!("active-{}", Uuid::new_v4().simple()),
        &plan_id,
        active,
    )
    .await;

    let routes = f.registry.get_routes().await.expect("get_routes");
    assert_eq!(
        routes.get(&suspended_app).expect("suspended app present").account_state,
        AccountState::Suspended,
        "an app must carry ITS organization's suspension"
    );
    assert_eq!(
        routes.get(&active_app).expect("active app present").account_state,
        AccountState::Active,
        "a suspension must not leak onto another organization's app"
    );

    drop(store);
    drop(f);
    common::drain_pg().await;
}

/// CRITICAL #1 regression — an out-of-order/redelivered `payment_failed` whose
/// Stripe `event.created` predates a recovery MUST NOT re-arm `past_due` on an
/// already-recovered (paying) organization, and the dunning sweep must therefore NOT
/// suspend them.
///
/// RED before the fix: `record_payment_failed` guarded only on `state`, so a
/// stale failure delivered AFTER recovery would flip active→past_due and the
/// sweep would suspend a paying organization. GREEN: the `last_recovered_at`
/// high-water (stamped from `event.created`) makes the stale failure a no-op.
#[compio::test]
async fn out_of_order_paid_then_failed_does_not_resuspend() {
    let _ = db_url();
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());
    let organization = make_organization(&f.pg).await;
    let organization = organization.as_str();

    // A real failure arms past_due at an EARLY event time.
    let early = T0;
    store
        .record_payment_failed(organization, Some("in_dunning"), early)
        .await
        .expect("failure")
        .expect("active→past_due");

    // Recovery (invoice.paid) lands with a LATER event.created → back to active,
    // stamps last_recovered_at = recover_at.
    let recover_at = T0 + 500;
    store
        .record_payment_recovered(organization, recover_at)
        .await
        .expect("recover")
        .expect("past_due→active");
    assert_eq!(db_state(&f.pg, organization).await.as_deref(), Some("active"));

    // Now a STALE/redelivered `payment_failed` for the SAME invoice arrives, but
    // its event.created PREDATES the recovery (early < recover_at). The
    // order-safety guard must IGNORE it: no transition, stays active, NOT re-armed.
    let stale = store
        .record_payment_failed(organization, Some("in_dunning"), early)
        .await
        .expect("stale failure handled");
    assert!(
        stale.is_none(),
        "a failure predating the recovery must NOT produce a transition"
    );
    assert_eq!(
        db_state(&f.pg, organization).await.as_deref(),
        Some("active"),
        "the stale failure must NOT re-arm past_due on a recovered organization"
    );
    // past_due_since must still be NULL (window not re-armed).
    let since: Option<chrono::DateTime<chrono::Utc>> = f
        .pg
        .query(
            "SELECT past_due_since FROM zeroship.organization_billing_status WHERE organization_id = $1",
            &[&organization],
        )
        .await
        .unwrap()[0]
        .get("past_due_since");
    assert!(since.is_none(), "dunning window must not be re-armed by a stale failure");

    // And the dunning sweep must NOT suspend this paying organization even if a
    // (hypothetically) old window existed — the guard kept state active.
    let transitions = store
        .suspend_exhausted(DEFAULT_MAX_DUNNING_DAYS)
        .await
        .expect("sweep");
    assert!(
        !transitions.iter().any(|t| t.organization_id == organization),
        "the dunning cron must NOT suspend a recovered organization hit by a stale failure"
    );
    assert_eq!(db_state(&f.pg, organization).await.as_deref(), Some("active"));

    // A FRESH failure (event.created AFTER the recovery) DOES legitimately re-arm
    // — the guard only blocks STALE events, not genuine post-recovery failures.
    let fresh_fail_at = recover_at + 100;
    let re = store
        .record_payment_failed(organization, Some("in_new"), fresh_fail_at)
        .await
        .expect("fresh failure")
        .expect("active→past_due on a genuine post-recovery failure");
    assert_eq!(re.to, AccountState::PastDue);
    assert_eq!(db_state(&f.pg, organization).await.as_deref(), Some("past_due"));

    drop(store);
    drop(f);
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// Redesign regression (change 7a): `organization_billing_status.organization_id` now FKs
// `organization_billing(organization_id)` (NOT users directly). A payment failure can be
// the FIRST billing signal for a organization with NO prior `organization_billing` row, so
// `record_payment_failed` must create the FK parent FIRST. (RED before the
// parent-first insert: the status INSERT FK-violates and the call errors.)
// ---------------------------------------------------------------------------

#[compio::test]
async fn payment_failed_creates_organization_billing_parent_first() {
    let _url = db_url();
    let f = fx().await;
    let store = AccountStatusStore::new(f.registry.clone());

    // A organization with a users row but NO organization_billing row yet (never ran
    // billing/setup) — the common "first billing signal is a failure" case.
    let organization = make_organization(&f.pg).await;
    let organization = organization.as_str();
    let pre = f
        .pg
        .query("SELECT 1 FROM zeroship.organization_billing WHERE organization_id = $1", &[&organization])
        .await
        .unwrap();
    assert!(pre.is_empty(), "precondition: no organization_billing row yet");

    // The failure must SUCCEED (parent-first), moving the organization to past_due.
    let t = store
        .record_payment_failed(organization, Some("in_first"), 1_000)
        .await
        .expect("payment failure must succeed even with no prior organization_billing row")
        .expect("active→past_due");
    assert_eq!(t.to, AccountState::PastDue);
    assert_eq!(db_state(&f.pg, organization).await.as_deref(), Some("past_due"));

    // The FK parent was created.
    let parent = f
        .pg
        .query("SELECT 1 FROM zeroship.organization_billing WHERE organization_id = $1", &[&organization])
        .await
        .unwrap();
    assert_eq!(parent.len(), 1, "record_payment_failed created the organization_billing FK parent");

    drop(store);
    drop(f);
    common::drain_pg().await;
}
