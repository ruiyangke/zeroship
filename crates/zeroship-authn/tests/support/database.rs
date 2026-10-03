//! One migrated platform database shared by every authn case in this process.
//!
//! The server is owned by [`zeroship_testkit::postgres`], started and migrated
//! the first time a case asks for it, and kept in a `static` libtest never
//! drops; the reaper removes it once this process has ended, however the
//! process ends.
//!
//! Cases share the server, so every case owns the rows and names it creates -
//! its ids, rate-limit keys, replay keys and per-case roles - and asserts only
//! on those. [`Database::run`] opens the case's connections against the shared
//! server and joins their driver tasks before the case's runtime ends.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use compio_postgres::Client;
use futures::FutureExt;
use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use zeroship_testkit::postgres::platform;

type Driver = compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>;

pub struct Database {
    drivers: RefCell<Vec<Driver>>,
}

impl Database {
    /// Run `test` against the shared migrated database.
    ///
    /// Connections the case opened are joined before its runtime ends, including
    /// when an assertion unwinds, and the case's own panic is re-raised after
    /// that.
    pub async fn run(test: impl AsyncFnOnce(&Self)) {
        let database = Self {
            drivers: RefCell::default(),
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(90), Box::pin(test(&database)))
                .await
                .expect("authn database case timed out");
        })
        .catch_unwind()
        .await;

        // The case's clients and HTTP services drop before we wait on their
        // exact connection tasks, including when an assertion unwinds.
        let drivers = database.drivers.take();
        let closed = compio::time::timeout(
            Duration::from_secs(15),
            futures::future::join_all(drivers),
        )
        .await;
        for driver in closed.expect("fixture connections must close before their runtime") {
            driver
                .expect("fixture driver task")
                .expect("fixture PostgreSQL connection");
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    pub async fn connect(&self) -> Client {
        let (client, driver) = platform().admin_connect().await;
        self.drivers.borrow_mut().push(driver);
        client
    }

    pub async fn connect_as(&self, role: &str) -> Client {
        let (client, driver) = platform().connect(role).await;
        self.drivers.borrow_mut().push(driver);
        client
    }

    /// Observe all requested backends waiting on locks before releasing a fixture transaction.
    #[allow(
        clippy::future_not_send,
        reason = "the database belongs to this compio runtime"
    )]
    pub async fn wait_until_blocked(&self, pids: &[i32]) -> bool {
        assert!(
            !pids.is_empty(),
            "a lock observation needs waiting backends"
        );
        let observer = self.connect().await;
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let blocked: bool = observer
                    .query_one(
                        "SELECT bool_and(cardinality(pg_blocking_pids(pid)) > 0) \
                         FROM unnest($1::int[]) AS requested(pid)",
                        &[&pids],
                    )
                    .await
                    .expect("observe fixture lock waiters")
                    .get(0);
                if blocked {
                    return;
                }
                compio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .is_ok()
    }
}
