//! The shared database fixture's own contracts, in the integration target.
//!
//! A case's connections are joined before its runtime ends, including when the
//! case unwinds, and a failed case re-raises its own failure rather than a
//! teardown symptom. An HTTP fixture the case never exercises still releases the
//! connection its app state holds before the case ends.

use futures::FutureExt;
use std::cell::RefCell;
use std::panic::AssertUnwindSafe;

use crate::support::database::Database;
use zeroship_core::UserId;

#[ntex::test]
async fn a_failed_case_re_raises_its_failure_and_closes_its_connections() {
    let role = format!("auth_failed_{}", UserId::mint().as_str());
    let failure = AssertUnwindSafe(Database::run(async |database| {
        let admin = database.connect().await;
        admin
            .batch_execute(&format!(
                "CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}'"
            ))
            .await
            .unwrap();
        let _case = database.connect_as(&role).await;
        let open: i64 = admin
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE usename = $1",
                &[&role],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            open > 0,
            "the failed case must hold its own connection before it unwinds"
        );
        panic!("intentional fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("a failed case must re-raise its own panic");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"intentional fixture failure")
    );

    let mut open_after = None;
    Database::run(async |database| {
        let admin = database.connect().await;
        open_after = Some(
            admin
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE usename = $1",
                    &[&role],
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
        );
        admin
            .batch_execute(&format!("DROP ROLE \"{role}\""))
            .await
            .unwrap();
    })
    .await;
    assert_eq!(
        open_after,
        Some(0),
        "a failed case must close its connections before re-raising"
    );
}

/// A case that fails reports its own failure even when its teardown also
/// fails, and a case that passes still fails on a teardown it left broken.
///
/// Each case moves a client out of itself, so its connection outlives the
/// case and the teardown's close check fails for both.
#[ntex::test]
async fn a_failed_case_reports_its_own_failure_over_the_teardown_it_broke() {
    let leaked = RefCell::new(Vec::new());
    let failed = AssertUnwindSafe(Database::run(async |database| {
        let client = database.connect().await;
        leaked.borrow_mut().push(client);
        panic!("intentional fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("case must propagate its assertion failure");
    assert_eq!(
        failed.downcast_ref::<&str>(),
        Some(&"intentional fixture failure"),
        "the teardown replaced the case's own failure: {:?}",
        failed.downcast_ref::<String>()
    );

    let passed = AssertUnwindSafe(Database::run(async |database| {
        let client = database.connect().await;
        leaked.borrow_mut().push(client);
    }))
    .catch_unwind()
    .await
    .expect_err("a passing case must still fail on the connection it left open");
    let message = passed
        .downcast_ref::<String>()
        .expect("the teardown reports what it found");
    assert!(
        message.contains("fixture connections must close before their runtime"),
        "{message}"
    );
    assert_eq!(leaked.borrow().len(), 2, "both cases leaked their client");
}

/// An HTTP fixture the case never exercises still releases the connection its
/// app state holds before the case ends.
///
/// The server registers its worker after the listener is bound. A case that
/// ends first leaves that worker outside the stop, and the app state it owns
/// keeps the fixture's client alive past the case and its teardown.
#[ntex::test]
async fn an_unexercised_auth_server_releases_its_connection_before_the_case_ends() {
    crate::support::database::Database::run(async |database| {
        let _server = crate::support::auth_server::AuthServer::start(database).await;
    })
    .await;
}
