//! The migrated platform database a gateway unit case runs against.
//!
//! Every case in this binary shares one reaper-owned `PostgreSQL` server
//! through [`zeroship_testkit::postgres`], migrated once. A case that owns only
//! the rows and names it mints runs on that shared database; a case whose
//! subject is platform-global - a schema rename, a table-wide lock, the
//! signing-key registry - gets its own database cloned from the
//! connection-free migrated template with [`Database::run_fresh`].
//!
//! Connections the case opens are joined before its runtime ends, including
//! when an assertion unwinds, and the case's own panic is re-raised after that.
//! The runner lives in the testkit; this is the gateway-specific adapter over
//! it.

use compio_postgres::Client;
use std::cell::{OnceCell, RefCell};

use zeroship_testkit::postgres::{Case, CaseFixture};

use super::super::{DbConfig, POOL};

/// The migrated platform database one gateway case runs against.
pub struct Database {
    case: Case,
    /// The case's superuser connection, opened once before its body runs.
    ///
    /// Its driver is held here rather than registered with the case: the
    /// connection lives as long as the fixture, so the runner's connection
    /// join would wait on a client the fixture still owns.
    admin: OnceCell<Client>,
    admin_driver: RefCell<Option<AdminDriver>>,
}

type AdminDriver = compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>;

impl CaseFixture for Database {
    fn from_case(case: Case) -> Self {
        Self {
            case,
            admin: OnceCell::new(),
            admin_driver: RefCell::new(None),
        }
    }

    fn case(&self) -> &Case {
        &self.case
    }
}

impl std::ops::Deref for Database {
    type Target = Case;

    fn deref(&self) -> &Self::Target {
        &self.case
    }
}

impl Database {
    /// Run `test` against the shared migrated database.
    #[expect(
        clippy::future_not_send,
        reason = "fixtures belong to their compio runtime"
    )]
    pub async fn run(test: impl AsyncFnOnce(&Self)) {
        zeroship_testkit::postgres::run::<Self>(async |database| {
            database.initialize().await;
            database.exercise(test).await;
        })
        .await;
    }

    /// Run `test` against a database cloned from the migrated template.
    #[expect(
        clippy::future_not_send,
        reason = "fixtures belong to their compio runtime"
    )]
    pub async fn run_fresh(test: impl AsyncFnOnce(&Self)) {
        zeroship_testkit::postgres::run_fresh::<Self>(async |database| {
            database.initialize().await;
            database.exercise(test).await;
        })
        .await;
    }

    /// Run the case body and close this thread's gateway pool afterwards, even
    /// when the body unwinds, so a later case on the same thread cannot inherit
    /// a pool bound to this case's database.
    #[expect(
        clippy::future_not_send,
        reason = "fixtures belong to their compio runtime"
    )]
    async fn exercise(&self, test: impl AsyncFnOnce(&Self)) {
        use futures::FutureExt;
        use std::panic::AssertUnwindSafe;

        let outcome = AssertUnwindSafe(async { test(self).await })
            .catch_unwind()
            .await;
        if let Some((_, pool)) = POOL.with(|slot| slot.borrow_mut().take()) {
            pool.close().await;
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// Open the case's superuser connection before its body runs, so
    /// [`Database::admin`] can hand out a reference rather than connect per
    /// call.
    #[expect(
        clippy::future_not_send,
        reason = "fixtures belong to their compio runtime"
    )]
    async fn initialize(&self) {
        if self.admin.get().is_none() {
            let (client, driver) = zeroship_testkit::postgres::connect(self.base_url()).await;
            *self.admin_driver.borrow_mut() = Some(driver);
            let _ = self.admin.set(client);
        }
    }

    /// The case's superuser connection, for seeding and independent
    /// observation outside tenant filtering.
    #[must_use]
    pub fn admin(&self) -> &Client {
        self.admin
            .get()
            .expect("the case runner opens the admin connection before the body")
    }

    #[must_use]
    pub(crate) fn config(&self, capacity: usize) -> DbConfig {
        DbConfig::new(self.base_url().as_str(), capacity)
    }

    #[must_use]
    pub(crate) fn config_as(&self, role: &str, capacity: usize) -> DbConfig {
        let mut url = self.base_url().clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        DbConfig::new(url.as_str(), capacity)
    }

    #[expect(
        clippy::future_not_send,
        reason = "fixtures belong to their compio runtime"
    )]
    pub(crate) async fn connect_as(&self, role: &str) -> Client {
        let mut url = self.base_url().clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        self.connect_to(url.as_str()).await
    }

    #[expect(
        clippy::future_not_send,
        reason = "fixtures belong to their compio runtime"
    )]
    pub(crate) async fn wait_until_blocked(&self, pids: &[i32]) {
        assert!(
            self.case.wait_until_blocked(pids).await,
            "all pool queries must reach the database lock"
        );
    }
}

#[compio::test]
async fn a_failed_case_re_raises_its_failure_and_closes_its_connections() {
    use super::super::checkout;
    use futures::FutureExt;
    use std::panic::AssertUnwindSafe;
    use zeroship_core::UserId;

    let role = format!("gateway_failed_{}", UserId::mint().as_str());
    let failure = AssertUnwindSafe(Database::run(async |database| {
        let admin = database.admin();
        admin
            .batch_execute(&format!(
                "CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}'"
            ))
            .await
            .unwrap();
        let pool = checkout(&database.config_as(&role, 1)).await.unwrap();
        let _lease = pool.acquire().await.unwrap();
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
    assert!(POOL.with(|pool| pool.borrow().is_none()));

    Database::run(async |database| {
        let admin = database.admin();
        let open: i64 = admin
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE usename = $1",
                &[&role],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            open, 0,
            "a failed case must close its connections before re-raising"
        );
        admin
            .batch_execute(&format!("DROP ROLE \"{role}\""))
            .await
            .unwrap();
    })
    .await;
}

#[compio::test]
async fn fresh_cases_isolate_schema_changes_from_the_shared_database() {
    use super::super::checkout;

    Database::run_fresh(async |database| {
        database
            .admin()
            .batch_execute(
                "ALTER TABLE zeroship.app_user_identities RENAME TO fixture_fresh_identities",
            )
            .await
            .unwrap();
        let renamed: bool = database
            .admin()
            .query_one(
                "SELECT to_regclass('zeroship.fixture_fresh_identities') IS NOT NULL",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(renamed, "the fresh case owns the schema it changed");
    })
    .await;

    Database::run(async |database| {
        let present: bool = database
            .admin()
            .query_one(
                "SELECT to_regclass('zeroship.app_user_identities') IS NOT NULL",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            present,
            "a fresh case's schema change must not reach the shared database"
        );
        let pool = checkout(&database.config_as("zeroship_gateway", 1))
            .await
            .unwrap();
        let connection = pool.acquire().await.unwrap();
        assert_eq!(
            connection
                .query_one("SELECT current_user::text", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            "zeroship_gateway"
        );
        connection
            .query("SELECT client_id FROM zeroship.token_revocations", &[])
            .await
            .expect("restored gateway role can read revocations");
    })
    .await;
}