//! The one migrated platform database this test binary shares, and each case's
//! pair of connections to it.
//!
//! The server is owned by [`zeroship_testkit::postgres`], started and migrated
//! the first time a case asks for it; see that module for why it is removed
//! once this process has ended however the process ends.

#![allow(
    clippy::future_not_send,
    reason = "fixtures stay on their compio runtime"
)]

use compio_postgres::Client;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use zeroship_testkit::postgres::platform;

pub struct Database {
    pub admin: Client,
    pub service: Client,
    admin_driver: compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
    service_driver: compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>,
}

impl Database {
    pub async fn run(test: impl AsyncFnOnce(&Self)) {
        let (admin, admin_driver) = platform().admin_connect().await;
        let (service, service_driver) = platform().connect("zeroship_control").await;
        let database = Self {
            admin,
            service,
            admin_driver,
            service_driver,
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(45), Box::pin(async {
                test(&database).await;
            }))
            .await
            .expect("authorization database case timed out");
        })
        .catch_unwind()
        .await;

        let Self {
            admin,
            service,
            admin_driver,
            service_driver,
        } = database;
        drop(service);
        let service_closed = compio::time::timeout(Duration::from_secs(15), service_driver).await;
        drop(admin);
        let admin_closed = compio::time::timeout(Duration::from_secs(15), admin_driver).await;
        service_closed
            .expect("service connection timed out")
            .expect("service task")
            .expect("service connection");
        admin_closed
            .expect("observer connection timed out")
            .expect("observer task")
            .expect("observer connection");
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }
}

/// The child test the lifetime measurements drive, by its full path in this binary.
const CHILD_TEST: &str = "tests::database::the_shared_database_reports_its_container";

#[test]
fn the_shared_database_reports_its_container() {
    zeroship_testkit::lifetime::report_owner();
    let id = zeroship_testkit::postgres::server_container_id();
    zeroship_testkit::lifetime::report_container(&id);
}

#[test]
fn the_shared_database_is_removed_when_its_process_ends() {
    zeroship_testkit::lifetime::assert_removed_after_the_child_exits(CHILD_TEST);
}

#[test]
fn the_shared_database_is_removed_when_its_process_is_killed_while_starting() {
    zeroship_testkit::lifetime::assert_removed_after_a_kill_during_startup(CHILD_TEST);
}

#[compio::test]
async fn a_failed_case_re_raises_its_failure_and_closes_its_connections() {
    use std::cell::RefCell;
    let pids = RefCell::new(Vec::new());
    let failure = AssertUnwindSafe(Database::run(async |database| {
        let admin: i32 = database
            .admin
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let service: i32 = database
            .service
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        *pids.borrow_mut() = vec![admin, service];
        panic!("intentional authorization fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("propagate the case failure");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"intentional authorization fixture failure")
    );
    let failed_pids = pids.borrow().clone();
    Database::run(async |database| {
        let live: i64 = database
            .admin
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE pid = ANY($1)",
                &[&failed_pids],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            live, 0,
            "a failed case must close the connections it opened"
        );
    })
    .await;
}
