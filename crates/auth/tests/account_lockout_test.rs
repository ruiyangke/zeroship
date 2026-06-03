//! Account-lockout regression (review finding L5).
//!
//! `users.locked_until` was read by the eligibility / credential paths but
//! never SET outside a test fixture — leaky-bucket rate limiting was the sole
//! online-guessing defense. This test pins the real per-user lockout:
//!
//! 1. N consecutive wrong-password attempts set `users.locked_until` and bump
//!    `users.failed_login_count`.
//! 2. The next attempt — even with the CORRECT password — is rejected as
//!    `Ineligible` (locked), and `verify_password_credentials` never returns a
//!    `VerifiedUser` while locked.
//! 3. A successful login (after the lock window) resets the failure counter and
//!    clears `locked_until`.
//!
//! Pre-fix this FAILS: `record_login_failure` / `reset_login_failures` and the
//! `failed_login_count` column do not exist, so the lock is never set and step
//! (2) returns `InvalidCredentials` (the correct password verifies against a
//! never-locked row).
//!
//! Env-gated on `AUTH_DB_URL` (same gate as the sibling DB tests): the failure
//! counter is durable PG state, so the live path needs a real database.

use std::sync::Arc;

use compio_postgres::{connect, NoTls};
use ntex::web::test::TestRequest;
use uuid::Uuid;

use zeroship_auth::identity::credentials::{verify_password_credentials, CredentialError};
use zeroship_auth::identity::eligibility::{self, LoginIneligible};
use zeroship_auth::identity::{password, password_reset};
use zeroship_auth::store::users;

/// Lockout engages on the 5th consecutive failure (see
/// `credentials::LOCKOUT_THRESHOLD`). Keep this in sync with the impl.
const LOCKOUT_THRESHOLD: usize = 5;

const GOOD_PW: &str = "correct horse battery staple lockout";
const BAD_PW: &str = "definitely the wrong password here";

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn consecutive_failures_lock_account_then_success_resets() {
    let Ok(db_url) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping account_lockout_test (no AUTH_DB_URL)");
        return;
    };
    let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[account_lockout_test] pg connection driver: {e}");
        }
    })
    .detach();
    let pg = Arc::new(pg_client);

    let email = format!("lockout-{}@zeroship.test", Uuid::new_v4().simple());
    let phc = password::hash(GOOD_PW).expect("hash password");
    let user = users::create(&pg, &email, "Lockout Probe", Some(&phc))
        .await
        .expect("seed user");

    // Distinct IP per attempt so the per-IP / email+IP leaky buckets (cap 5)
    // never throttle before the lockout threshold is reached. The per-email
    // bucket (cap 10) still has headroom for THRESHOLD+1 attempts.
    let req = TestRequest::default().to_http_request();
    let ip_for = |n: usize| format!("203.0.113.{}", n + 1);

    // 1. Drive THRESHOLD consecutive wrong-password attempts.
    for n in 0..LOCKOUT_THRESHOLD {
        let err = verify_password_credentials(&pg, &req, "test-client", &ip_for(n), &email, BAD_PW)
            .await
            .expect_err("wrong password must fail");
        // Each pre-lock failure is an opaque invalid-credentials rejection.
        assert_eq!(
            err,
            CredentialError::InvalidCredentials,
            "attempt {n}: pre-lock wrong password should be InvalidCredentials, got {err:?}"
        );
    }

    // After THRESHOLD failures the row must be locked.
    let locked_until: Option<chrono::DateTime<chrono::Utc>> = pg
        .query_one(
            "SELECT locked_until FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .expect("query locked_until")
        .get("locked_until");
    assert!(
        locked_until.is_some_and(|t| t > chrono::Utc::now()),
        "after {LOCKOUT_THRESHOLD} consecutive failures locked_until must be set in the future, got {locked_until:?}"
    );

    // 2. The next attempt — WITH THE CORRECT PASSWORD — is rejected as locked.
    //    Pre-fix this returns Ok(VerifiedUser) (lock never set) → test fails.
    let err = verify_password_credentials(
        &pg,
        &req,
        "test-client",
        &ip_for(LOCKOUT_THRESHOLD),
        &email,
        GOOD_PW,
    )
    .await
    .expect_err("correct password on a locked account must STILL fail");
    assert_eq!(
        err,
        CredentialError::Ineligible,
        "locked account must reject even the correct password as Ineligible, got {err:?}"
    );

    // 3. Clear the lock window (simulate it elapsing) and verify a successful
    //    login resets the failure counter back to 0.
    pg.execute(
        "UPDATE zeroship.users SET locked_until = NOW() - INTERVAL '1 second' WHERE id = $1",
        &[&user.id],
    )
    .await
    .expect("expire lock window");

    let verified = verify_password_credentials(
        &pg,
        &req,
        "test-client",
        &ip_for(LOCKOUT_THRESHOLD + 1),
        &email,
        GOOD_PW,
    )
    .await
    .expect("correct password after lock window must succeed");
    assert_eq!(verified.id, user.id, "verified user id mismatch");

    let (count, locked): (i32, Option<chrono::DateTime<chrono::Utc>>) = {
        let row = pg
            .query_one(
                "SELECT failed_login_count, locked_until FROM zeroship.users WHERE id = $1",
                &[&user.id],
            )
            .await
            .expect("query post-success state");
        (row.get("failed_login_count"), row.get("locked_until"))
    };
    assert_eq!(count, 0, "successful login must reset failed_login_count to 0");
    assert!(
        locked.is_none(),
        "successful login must clear locked_until, got {locked:?}"
    );

    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

/// Connect + spawn the PG driver, returning a shared client. Returns `None`
/// (and prints a skip note) when `AUTH_DB_URL` is unset.
async fn connect_pg(label: &'static str) -> Option<Arc<compio_postgres::Client>> {
    let Ok(db_url) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping {label} (no AUTH_DB_URL)");
        return None;
    };
    let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[{label}] pg connection driver: {e}");
        }
    })
    .detach();
    Some(Arc::new(pg_client))
}

/// Lock a real user by driving THRESHOLD consecutive wrong-password attempts
/// through the production `verify_password_credentials` path. Asserts the row
/// is locked afterwards. (Distinct IPs avoid the per-IP leaky bucket.)
async fn lock_account_via_failures(
    pg: &compio_postgres::Client,
    email: &str,
    user_id: Uuid,
) {
    let req = TestRequest::default().to_http_request();
    for n in 0..LOCKOUT_THRESHOLD {
        let _ = verify_password_credentials(
            pg,
            &req,
            "test-client",
            &format!("198.51.100.{}", n + 1),
            email,
            BAD_PW,
        )
        .await;
    }
    let locked_until: Option<chrono::DateTime<chrono::Utc>> = pg
        .query_one(
            "SELECT locked_until FROM zeroship.users WHERE id = $1",
            &[&user_id],
        )
        .await
        .expect("query locked_until")
        .get("locked_until");
    assert!(
        locked_until.is_some_and(|t| t > chrono::Utc::now()),
        "setup: account must be locked after {LOCKOUT_THRESHOLD} failures, got {locked_until:?}"
    );
}

/// F2 regression — a locked account must RECOVER via a successful password
/// reset. An attacker who knows the victim's email can lock the account
/// (`locked_until` exponential backoff, ~1 req/hr sustains it). The lockout was
/// cleared ONLY by `reset_login_failures` on a successful PASSWORD login —
/// unreachable while locked. So a completed password reset (strong owner
/// evidence: the reset link was delivered to and redeemed from the verified
/// inbox) left the victim permanently locked.
///
/// This drives the REAL production flow end to end: lock via
/// `verify_password_credentials`, recover via `password_reset::{issue, redeem,
/// complete}`, then assert the NEW password verifies. It does NOT pre-seed the
/// cleared state.
///
/// Pre-fix: `complete`'s UPDATE touches only `password_hash` /
/// `credential_version`, so `locked_until` stays set and the post-reset login
/// is rejected `Ineligible`. Post-fix: `complete` also clears the soft lockout
/// and the login succeeds.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn locked_account_recovers_via_password_reset() {
    let Some(pg) = connect_pg("locked_account_recovers_via_password_reset").await else {
        return;
    };

    let email = format!("lockout-reset-{}@zeroship.test", Uuid::new_v4().simple());
    let old_phc = password::hash(GOOD_PW).expect("hash old password");
    let user = users::create(&pg, &email, "Lockout Reset Probe", Some(&old_phc))
        .await
        .expect("seed user");

    lock_account_via_failures(&pg, &email, user.id).await;

    // Real password-reset completion flow (mirrors the `/reset` POST handler in
    // `ui/reset.rs`, which calls `complete` directly — `redeem` is only the GET
    // pre-check and would consume the row): issue → complete with a NEW password.
    const NEW_PW: &str = "brand new recovery password 2026";
    let issued = password_reset::issue(&pg, &email)
        .await
        .expect("issue reset token");
    let new_phc = password::hash(NEW_PW).expect("hash new password");
    let completed = password_reset::complete(&pg, &issued.raw, &new_phc)
        .await
        .expect("complete reset")
        .expect("reset must complete");
    assert_eq!(completed.user_id, user.id, "completed reset user mismatch");

    // The account must now be recoverable: the NEW password verifies.
    // Pre-fix this is rejected `Ineligible` (locked_until still set).
    let req = TestRequest::default().to_http_request();
    let verified = verify_password_credentials(
        &pg,
        &req,
        "test-client",
        "198.51.100.200",
        &email,
        NEW_PW,
    )
    .await
    .expect("post-reset login with the NEW password must succeed (account recovered)");
    assert_eq!(verified.id, user.id, "recovered user id mismatch");

    // And the lockout state is fully cleared.
    let (count, locked): (i32, Option<chrono::DateTime<chrono::Utc>>) = {
        let row = pg
            .query_one(
                "SELECT failed_login_count, locked_until FROM zeroship.users WHERE id = $1",
                &[&user.id],
            )
            .await
            .expect("query post-reset state");
        (row.get("failed_login_count"), row.get("locked_until"))
    };
    assert_eq!(count, 0, "password reset must zero failed_login_count");
    assert!(
        locked.is_none(),
        "password reset must clear locked_until, got {locked:?}"
    );

    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

/// F2 regression — the magic-link and OAuth success paths share the
/// `eligibility::check_user_eligible` gate, which treats a soft failed-login
/// lockout (`locked_until`) identically to a hard `disabled_at`. So a victim
/// locked by password-guessing could not log in via magic-link or OAuth either
/// (verified-email / federated login is strong owner-present evidence and must
/// recover the account).
///
/// The fix clears the soft lockout immediately before that gate on the
/// magic/oauth/link success paths. This test pins the recovery property at the
/// shared production gate: after clearing the lockout (as the success paths now
/// do), the SAME `check_user_eligible` call a locked account previously failed
/// now passes — while a hard `disabled_at` is still rejected (the lockout fix
/// must NOT weaken the disable gate).
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn locked_account_recovers_at_eligibility_gate_after_lockout_clear() {
    let Some(pg) =
        connect_pg("locked_account_recovers_at_eligibility_gate_after_lockout_clear").await
    else {
        return;
    };

    let email = format!("lockout-elig-{}@zeroship.test", Uuid::new_v4().simple());
    let phc = password::hash(GOOD_PW).expect("hash password");
    let user = users::create(&pg, &email, "Lockout Elig Probe", Some(&phc))
        .await
        .expect("seed user");

    lock_account_via_failures(&pg, &email, user.id).await;

    // Baseline: while locked, the shared gate rejects (Locked). This is the
    // gate magic-link / OAuth success hit BEFORE minting a session.
    match eligibility::check_user_eligible(&pg, user.id).await {
        Err(LoginIneligible::Locked) => {}
        other => panic!("locked account should fail eligibility as Locked, got {other:?}"),
    }

    // The magic/oauth/link success paths now clear the soft lockout (strong
    // owner-present evidence) before this gate. Exercise that production store
    // call, then re-run the SAME gate.
    users::reset_login_failures(&pg, user.id)
        .await
        .expect("clear soft lockout");
    eligibility::check_user_eligible(&pg, user.id)
        .await
        .expect("after lockout clear the account must pass eligibility (recovered)");

    // The disable gate is independent and must STILL reject — clearing a soft
    // lockout must not resurrect a hard-disabled account.
    pg.execute(
        "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
        &[&user.id],
    )
    .await
    .expect("disable account");
    match eligibility::check_user_eligible(&pg, user.id).await {
        Err(LoginIneligible::Disabled) => {}
        other => panic!("disabled account must still be rejected, got {other:?}"),
    }

    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

/// F7 regression — the two password-failure arms must perform an EQUIVALENT
/// number of latency-visible DB round-trips, so the post-Argon2 DB work cannot
/// be used as an email-enumeration timing oracle.
///
/// The dummy-hash already equalizes the Argon2 wall time, but post-verify the
/// real-password wrong-password arm runs `record_login_failure` (a serialized
/// `users` UPDATE) BEFORE its audit row, while the absent / OAuth-only arm runs
/// only the audit INSERT. That extra round-trip makes real, password-bearing
/// accounts measurably slower — an enumeration channel.
///
/// This drives the REAL production path (`verify_password_credentials`) for
/// both arms and asserts they bump the shared `login_failure_roundtrips`
/// counter by the SAME amount — a structural assertion, not flaky wall-clock
/// timing. It does NOT pre-seed any state the bug hides behind.
///
/// Pre-fix: the absent arm issues ZERO failed-login round-trips (only the real
/// arm calls `record_login_failure`), so the deltas differ (1 vs 0) and this
/// FAILS. Post-fix: the absent arm issues one throwaway round-trip
/// (`record_login_failure_dummy`), so both deltas are 1.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn failure_arms_perform_equivalent_db_roundtrips() {
    let Some(pg) = connect_pg("failure_arms_perform_equivalent_db_roundtrips").await else {
        return;
    };

    // A real, password-bearing user for the wrong-password arm (arm 6b).
    let real_email = format!("f7-real-{}@zeroship.test", Uuid::new_v4().simple());
    let phc = password::hash(GOOD_PW).expect("hash password");
    let user = users::create(&pg, &real_email, "F7 Real Probe", Some(&phc))
        .await
        .expect("seed real user");

    let req = TestRequest::default().to_http_request();

    // ── Arm 6b: wrong password on a REAL user (ONE attempt, below the lock
    //    threshold, so `record_login_failure` issues exactly one UPDATE). ──
    let before_real = users::login_failure_roundtrips();
    let err = verify_password_credentials(
        &pg,
        &req,
        "test-client",
        "203.0.113.50",
        &real_email,
        BAD_PW,
    )
    .await
    .expect_err("wrong password must fail");
    assert_eq!(err, CredentialError::InvalidCredentials);
    let real_delta = users::login_failure_roundtrips() - before_real;

    // ── Arm 6a: any password on an ABSENT user. ──
    let ghost_email = format!("f7-ghost-{}@zeroship.test", Uuid::new_v4().simple());
    let before_ghost = users::login_failure_roundtrips();
    let err = verify_password_credentials(
        &pg,
        &req,
        "test-client",
        "203.0.113.51",
        &ghost_email,
        BAD_PW,
    )
    .await
    .expect_err("absent user must fail");
    assert_eq!(err, CredentialError::InvalidCredentials);
    let ghost_delta = users::login_failure_roundtrips() - before_ghost;

    // The single wrong-password attempt is below `lockout::THRESHOLD`, so the
    // real arm performs exactly one failed-login round-trip.
    assert_eq!(
        real_delta, 1,
        "real wrong-password arm should issue exactly one failed-login round-trip"
    );
    // The crux: the absent arm must perform the SAME number of latency-visible
    // failed-login round-trips. Pre-fix it is 0 (enumeration oracle).
    assert_eq!(
        ghost_delta, real_delta,
        "absent-user arm ({ghost_delta} round-trips) must match the real wrong-password \
         arm ({real_delta}); a divergence is a post-verify DB-timing enumeration oracle"
    );

    // ── Arm 5: locked/disabled real user must also match (no sibling oracle). ──
    pg.execute(
        "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
        &[&user.id],
    )
    .await
    .expect("disable user");
    let before_disabled = users::login_failure_roundtrips();
    let err = verify_password_credentials(
        &pg,
        &req,
        "test-client",
        "203.0.113.52",
        &real_email,
        BAD_PW,
    )
    .await
    .expect_err("disabled user must fail");
    assert_eq!(err, CredentialError::Ineligible);
    let disabled_delta = users::login_failure_roundtrips() - before_disabled;
    assert_eq!(
        disabled_delta, real_delta,
        "ineligible (locked/disabled) arm ({disabled_delta}) must match the wrong-password \
         arm ({real_delta}); a faster path here is a distinguishable account-state oracle"
    );

    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
