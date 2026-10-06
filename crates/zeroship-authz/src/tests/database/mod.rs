//! The one migrated platform database this test binary shares, and each case's
//! pair of connections to it.
//!
//! The server is owned by [`zeroship_testkit::postgres::platform`], booted once
//! per worktree and leased for the life of each process; see that module for why
//! the container's watchdog removes it once no process holds a lease.

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
        // Release BOTH sockets before polling either driver. A connection
        // queues its drop-time housekeeping (for example `Close(S)` for the
        // statement a query left cached) the moment the query returns, and the
        // connection task only writes that request when the runtime next polls
        // it. Awaiting one driver first is exactly such a poll: it flushes the
        // other connection's queued housekeeping to the server, so when that
        // other client is then dropped the server still holds its unread
        // reply and answers the close with a reset instead of a FIN. Dropping
        // every client up front runs each synchronous release (`Socket::shutdown`)
        // before any task can write, so the queued housekeeping is discarded
        // locally and the close stays clean.
        drop(admin);
        drop(service);
        let admin_closed = compio::time::timeout(Duration::from_secs(15), admin_driver).await;
        let service_closed = compio::time::timeout(Duration::from_secs(15), service_driver).await;
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
