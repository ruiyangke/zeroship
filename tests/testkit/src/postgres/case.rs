//! The generic database-case runner every crate's fixture adapts.
//!
//! A case runs against the process's shared migrated database, or against a
//! clone of the migrated template when its subject is platform-global rather
//! than scoped to the rows it mints. The runner opens the case's connections
//! through [`Case`], joins their driver tasks before the case's runtime ends,
//! and re-raises the case's own panic after reporting what the teardown found.
//!
//! The runner carries no domain types. A crate's fixture wraps [`Case`] and
//! implements [`CaseFixture`] to add its own connection helpers.

use compio_postgres::Client;
use futures::FutureExt;
use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use super::platform;

type Driver = compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>;

/// One case's connections to a database, joined when the case ends.
pub struct Case {
    base: url::Url,
    drivers: RefCell<Vec<Driver>>,
}

impl Case {
    fn new(base: url::Url) -> Self {
        Self {
            base,
            drivers: RefCell::default(),
        }
    }

    /// The database URL [`Case::connect`] targets.
    #[must_use]
    pub fn base_url(&self) -> &url::Url {
        &self.base
    }

    /// Open a connection to the case's database and track its driver task.
    pub async fn connect(&self) -> Client {
        self.connect_to(self.base.as_str()).await
    }

    /// Open a connection to `url` and track its driver task.
    pub async fn connect_to(&self, url: &str) -> Client {
        let parsed: url::Url = url.parse().expect("fixture database URL");
        let (client, driver) = super::connect(&parsed).await;
        self.drivers.borrow_mut().push(driver);
        client
    }

    /// Observe all requested backends waiting on locks before releasing a fixture transaction.
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

/// A crate's database fixture: the generic [`Case`] plus its own helpers.
pub trait CaseFixture: Sized {
    /// Wrap a fresh case's connections.
    fn from_case(case: Case) -> Self;

    /// The case's generic connections and database URL.
    fn case(&self) -> &Case;
}

/// Run `test` against the shared migrated database.
pub async fn run<F: CaseFixture>(test: impl AsyncFnOnce(&F)) {
    on::<F>(platform().admin_url(), test).await;
}

/// Run `test` against a database cloned from the migrated template.
pub async fn run_fresh<F: CaseFixture>(test: impl AsyncFnOnce(&F)) {
    let fresh = platform().fresh_database();
    let base = fresh.admin_url();
    on::<F>(base, test).await;
    // `fresh` drops here, after `on` joined the case's drivers, removing the
    // clone.
    drop(fresh);
}

async fn on<F: CaseFixture>(base: url::Url, test: impl AsyncFnOnce(&F)) {
    let database = F::from_case(Case::new(base));
    let outcome = AssertUnwindSafe(async {
        compio::time::timeout(Duration::from_secs(90), Box::pin(test(&database)))
            .await
            .expect("database case timed out");
    })
    .catch_unwind()
    .await;

    // The case's clients and HTTP services drop before we wait on their exact
    // connection tasks, including when an assertion unwinds.
    let drivers = database.case().drivers.take();
    let closed =
        compio::time::timeout(Duration::from_secs(15), futures::future::join_all(drivers)).await;
    let mut teardown = Vec::new();
    match closed {
        Err(elapsed) => teardown.push(format!(
            "fixture connections must close before their runtime: {elapsed:?}"
        )),
        Ok(drivers) => {
            for driver in drivers {
                match driver {
                    Err(_) => teardown.push("fixture driver task panicked".to_owned()),
                    Ok(Err(error)) => {
                        teardown.push(format!("fixture PostgreSQL connection: {error:?}"));
                    }
                    Ok(Ok(())) => {}
                }
            }
        }
    }
    // A case that failed reports its own failure. What its teardown then found
    // is printed beside it rather than raised instead of it: a case that
    // unwinds while the server is still working on its request can leave that
    // server's connection open, and the symptom must not replace the failure
    // that caused it.
    match outcome {
        Err(panic) => {
            for failure in &teardown {
                eprintln!("fixture teardown after the case failed: {failure}");
            }
            std::panic::resume_unwind(panic);
        }
        Ok(()) => assert!(teardown.is_empty(), "{}", teardown.join("; ")),
    }
}
