//! The database fixture's own lifecycle contracts, in the integration target.
//!
//! A case's PostgreSQL server is per-case and is torn down before the next
//! case starts; a failure must report the case's own error even when teardown
//! also fails. These run as `#[ntex::test]`s against `support::database`.

use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::process::Command;

use futures::FutureExt;

use crate::support::database::Database;

#[ntex::test]
async fn a_failed_case_releases_its_server_and_cannot_change_the_next_database() {
    let failed_id = RefCell::new(String::new());
    let failed = AssertUnwindSafe(Database::run(async |database| {
        *failed_id.borrow_mut() = database.container_id();
        assert!(container_ids().contains(&*failed_id.borrow()));
        let client = database.connect().await;
        client
            .batch_execute("CREATE TABLE fixture_isolation (id integer)")
            .await
            .unwrap();
        panic!("intentional fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("case must propagate its assertion failure");
    assert_eq!(
        failed.downcast_ref::<&str>(),
        Some(&"intentional fixture failure")
    );
    assert!(
        !container_ids().contains(&*failed_id.borrow()),
        "failed case leaked its PostgreSQL server"
    );

    let successful_id = RefCell::new(String::new());
    Database::run(async |database| {
        *successful_id.borrow_mut() = database.container_id();
        let client = database.connect().await;
        let absent: bool = client
            .query_one("SELECT to_regclass('fixture_isolation') IS NULL", &[])
            .await
            .unwrap()
            .get(0);
        assert!(absent, "a failed case changed the immutable seed");
        let auth = database.connect_as_auth().await;
        let role: String = auth
            .query_one("SELECT current_user::text", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(role, "zeroship_auth");
        auth.query("SELECT id FROM zeroship.users", &[])
            .await
            .expect("restored role can read the migrated auth schema");
    })
    .await;
    assert!(
        !container_ids().contains(&*successful_id.borrow()),
        "successful case leaked its PostgreSQL server"
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

fn container_ids() -> Vec<String> {
    let output = Command::new("docker")
        .args(["ps", "--all", "--quiet", "--no-trunc"])
        .output()
        .expect("query fixture container lifecycle");
    assert!(
        output.status.success(),
        "Docker container listing failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
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
