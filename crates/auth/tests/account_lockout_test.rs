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
use zeroship_auth::identity::password;
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
