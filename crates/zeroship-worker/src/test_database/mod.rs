//! `PostgreSQL` fixtures draw from the worktree's one shared migrated platform
//! server rather than booting a container of their own.
//!
//! [`Database::run`] clones the migrated template every worker test process of
//! the worktree shares ([`zeroship_testkit::postgres::platform`]); the clone
//! gives the case a database of its own, but `PostgreSQL` roles are
//! cluster-global (`pg_authid`), so a role the case creates is minted through
//! [`Database::mint_role`] and dropped at teardown, never given a fixed name
//! another case's process could collide with.
//!
//! [`Database::isolated`] instead boots a migrated server this case alone
//! holds the lease to, for a case whose assertion reaches `zeroship_worker`'s
//! own cluster-wide posture - its role attributes, or the membership rows the
//! fence check in [`crate::db_posture`] counts - rather than rows scoped to
//! what the case itself minted. Every worker test process of the worktree
//! shares one `zeroship_worker` login on the shared server, so a concurrent
//! case granting or revoking its memberships would race this one's read of
//! them; such a case takes a server of its own instead of weakening what it
//! asserts.

use compio_postgres::{Client, NoTls};
use futures::FutureExt;
use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use zeroship_shared_server::Scope;
use zeroship_testkit::postgres::{FreshDatabase, Platform};

type Driver = compio::runtime::JoinHandle<Result<(), compio_postgres::Error>>;

/// How long a private server outlives the case that owns it.
///
/// A case that joins [`Scope::private`] holds that server's only lease, so
/// once the case drops it the watchdog removes the server; the grace only has
/// to cover the gap between the case's last operation and that drop, not a
/// run's worth of idle time the way the shared platform's grace does.
const PRIVATE_GRACE: Duration = Duration::from_secs(2);

/// The private scope's kind, filed separately from the worktree's shared
/// `platform` scope so a private boot never collides with - or is mistaken
/// for - the shared server's own lease directory.
const PRIVATE_KIND: &str = "worker-posture";

/// The server a case's database lives on.
enum Owner {
    /// A clone of the worktree's shared migrated template.
    Shared(FreshDatabase),
    /// A migrated server this case alone holds the lease to.
    Private(Platform),
}

pub struct Database {
    owner: Owner,
    url: url::Url,
    pub(crate) admin: Client,
    driver: Driver,
    role_drivers: RefCell<Vec<Driver>>,
    /// Role names this case minted on the shared server, dropped at teardown
    /// before the clone that held their grants is removed. Empty on the
    /// private path: nothing else can see a private server's roles, so there
    /// is nothing another case needs protecting from.
    minted_roles: RefCell<Vec<String>>,
}

impl Database {
    /// Run `test` against a clone of the worktree's shared migrated platform.
    ///
    /// A role the test creates is cluster-global and this server is shared by
    /// every worker test process of the worktree, so mint its name through
    /// [`Database::mint_role`] rather than giving it a fixed one.
    pub(crate) async fn run(test: impl AsyncFnOnce(&Self)) {
        let fresh = zeroship_testkit::postgres::platform().fresh_database();
        let base = fresh.admin_url();
        Self::on(Owner::Shared(fresh), base, test).await;
    }

    /// Run `test` against a migrated server this case alone holds the lease
    /// to: see the module doc for why a posture case that reaches
    /// `zeroship_worker`'s own cluster-wide state takes a server of its own
    /// rather than the one every other worker test process shares.
    ///
    /// # Panics
    /// When the private server cannot be booted or migrated.
    pub(crate) async fn isolated(test: impl AsyncFnOnce(&Self)) {
        let platform = Platform::join(&Scope::private(PRIVATE_KIND, PRIVATE_GRACE))
            .unwrap_or_else(|error| {
                panic!(
                    "a private migrated server for the worker's database posture tests could \
                     not be started: {error}"
                )
            });
        let base = platform.admin_url();
        Self::on(Owner::Private(platform), base, test).await;
    }

    async fn on(owner: Owner, base: url::Url, test: impl AsyncFnOnce(&Self)) {
        let (admin, driver) = connect(&base).await;
        let database = Self {
            owner,
            url: base,
            admin,
            driver,
            role_drivers: RefCell::new(Vec::new()),
            minted_roles: RefCell::new(Vec::new()),
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(90), Box::pin(test(&database)))
                .await
                .expect("worker database case timed out");
        })
        .catch_unwind()
        .await;

        let drained = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(15), async {
                for driver in database.role_drivers.take() {
                    driver.await.expect("role driver task").expect("role connection");
                }
                loop {
                    // Scoped to this case's own database: `pg_stat_activity` is
                    // cluster-wide, and the shared server now carries every
                    // concurrent case's clone, so an unscoped read would never
                    // see "empty" while any other case was still working.
                    let empty: bool = database.admin.query_one(
                        "SELECT NOT EXISTS (SELECT FROM pg_stat_activity \
                         WHERE backend_type = 'client backend' AND pid <> pg_backend_pid() \
                         AND datname = current_database())",
                        &[],
                    ).await.expect("observe connection cleanup").get(0);
                    if empty { break; }
                    compio::time::sleep(Duration::from_millis(25)).await;
                }
            }).await.expect("connections must close before removing the owned database");
        }).catch_unwind().await;

        // Every role this case minted is dropped here, on its own and
        // regardless of whether the drain above succeeded: `PostgreSQL` does
        // not require a role's own session to be closed before `DROP ROLE`,
        // so a case must not leak its role just because something else in
        // teardown ran long. `DROP OWNED BY` reaches cluster-wide membership
        // grants regardless of which database issues it, so this still runs
        // before the clone (or the private server) that held them goes away.
        //
        // Every minted role is attempted, not just attempted-until-the-first-
        // failure: a role can own something outside what this case's own
        // `DROP OWNED BY` reaches (an object in another database, say), and
        // one role failing to drop must not abandon the roles minted after it
        // to leak onto the shared server unattempted.
        let roles_dropped = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(15), async {
                let mut failures = Vec::new();
                for role in database.minted_roles.take() {
                    if let Err(error) = database
                        .admin
                        .batch_execute(&format!(
                            "DROP OWNED BY \"{role}\"; DROP ROLE IF EXISTS \"{role}\""
                        ))
                        .await
                    {
                        failures.push(format!("{role}: {error}"));
                    }
                }
                assert!(
                    failures.is_empty(),
                    "minted role(s) could not be dropped and leak onto the shared server: {}",
                    failures.join("; ")
                );
            }).await.expect("minted roles must drop before removing the owned database");
        }).catch_unwind().await;

        let Self {
            owner,
            admin,
            driver,
            ..
        } = database;
        drop(admin);
        let closed = compio::time::timeout(Duration::from_secs(15), driver).await;
        drop(owner);
        if let Err(panic) = outcome {
            if drained.is_err() || roles_dropped.is_err() || !matches!(closed, Ok(Ok(Ok(())))) {
                eprintln!("worker fixture cleanup also failed; preserving the case failure");
            }
            std::panic::resume_unwind(panic);
        }
        if let Err(panic) = drained {
            std::panic::resume_unwind(panic);
        }
        if let Err(panic) = roles_dropped {
            std::panic::resume_unwind(panic);
        }
        closed
            .expect("observer connection timed out")
            .expect("observer task")
            .expect("observer connection");
    }

    /// A role name unique to this case.
    ///
    /// `PostgreSQL` roles are cluster-global (`pg_authid`), and [`Database::run`]'s
    /// server is shared by every worker test process of the worktree, so a
    /// fixed name would let one case's grants - or a role-membership mutation -
    /// reach another case's session. Tracked for [`Database::on`]'s teardown,
    /// which drops it before this case's clone is removed.
    pub(crate) fn mint_role(&self, label: &str) -> String {
        let role = format!("{label}_{}", zeroship_core::typed_id::generate("wkt"));
        self.minted_roles.borrow_mut().push(role.clone());
        role
    }

    /// The Docker id of the server this case's database lives on, so a
    /// contract can show two [`Database::run`] cases of a run share one
    /// container.
    pub(crate) fn container_id(&self) -> &str {
        match &self.owner {
            Owner::Shared(fresh) => fresh.container_id(),
            Owner::Private(platform) => platform.container_id(),
        }
    }

    /// This case's own database name, so a contract can show two
    /// [`Database::run`] cases still own a database each.
    pub(crate) fn database_name(&self) -> &str {
        self.url.path().trim_start_matches('/')
    }

    pub(crate) fn url_as(&self, role: &str) -> url::Url {
        let mut url = self.url.clone();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        url
    }

    pub(crate) async fn connect_as(&self, role: &str) -> Client {
        let (client, driver) = connect(&self.url_as(role)).await;
        self.role_drivers.borrow_mut().push(driver);
        client
    }
}

async fn connect(url: &url::Url) -> (Client, Driver) {
    let mut config: compio_postgres::Config = url.as_str().parse().unwrap();
    config.connect_timeout(Duration::from_secs(10));
    let (client, connection) = config.connect(NoTls).await.expect("connect worker fixture");
    (
        client,
        compio::runtime::spawn(async move { connection.run().await }),
    )
}

#[compio::test]
async fn a_failed_case_drops_its_own_clone_and_role_without_touching_anothers() {
    Database::run(async |other| {
        let other_role = other.mint_role("other_role");
        other
            .admin
            .batch_execute(&format!(
                "CREATE ROLE \"{other_role}\" LOGIN PASSWORD '{other_role}'"
            ))
            .await
            .unwrap();
        let failed_database_name = RefCell::new(String::new());
        let failed_role = RefCell::new(String::new());
        let failure = AssertUnwindSafe(Database::run(async |database| {
            *failed_database_name.borrow_mut() = database.database_name().to_owned();
            let role = database.mint_role("failed_role");
            *failed_role.borrow_mut() = role.clone();
            database
                .admin
                .batch_execute(&format!("CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}'"))
                .await
                .unwrap();
            let client = database.connect_as(&role).await;
            client.query_one("SELECT current_user", &[]).await.unwrap();
            panic!("intentional worker fixture failure");
        }))
        .catch_unwind()
        .await
        .expect_err("propagate case failure");
        assert_eq!(
            failure.downcast_ref::<&str>(),
            Some(&"intentional worker fixture failure")
        );

        let failed_database_name = failed_database_name.into_inner();
        let failed_role = failed_role.into_inner();
        let remaining_database = other
            .admin
            .query(
                "SELECT 1 FROM pg_database WHERE datname = $1",
                &[&failed_database_name],
            )
            .await
            .unwrap();
        assert!(
            remaining_database.is_empty(),
            "a failed case must drop its own clone"
        );
        let remaining_role = other
            .admin
            .query(
                "SELECT 1 FROM pg_roles WHERE rolname = $1",
                &[&failed_role],
            )
            .await
            .unwrap();
        assert!(
            remaining_role.is_empty(),
            "a failed case must drop its own minted role"
        );

        let client = other.connect_as(&other_role).await;
        assert_eq!(
            client
                .query_one("SELECT current_user", &[])
                .await
                .unwrap()
                .get::<_, String>(0),
            other_role,
            "the other case's own role must survive the failed case's teardown"
        );
    })
    .await;
}

/// Before the shared migrated server, `Database::run` booted a dedicated
/// `testcontainers` container per case, so this equality did not hold; it is
/// the one assertion this file makes that the earlier fixture could not have
/// passed.
#[compio::test]
async fn two_cases_share_one_container_but_mint_distinct_roles() {
    Database::run(async |first| {
        Database::run(async |second| {
            assert_eq!(
                first.container_id(),
                second.container_id(),
                "two Database::run cases of a run must clone from the same shared server"
            );
            assert_ne!(
                first.database_name(),
                second.database_name(),
                "each case must still own a database of its own"
            );
            let first_role = first.mint_role("fixture_role");
            let second_role = second.mint_role("fixture_role");
            assert_ne!(
                first_role, second_role,
                "two cases minting the same label must not collide on a name"
            );
            first
                .admin
                .batch_execute(&format!("CREATE ROLE \"{first_role}\" NOLOGIN"))
                .await
                .unwrap();
            second
                .admin
                .batch_execute(&format!("CREATE ROLE \"{second_role}\" NOLOGIN"))
                .await
                .unwrap();
        })
        .await;
    })
    .await;
}

/// A role minted alongside another can fail to drop - it owns a schema on a
/// different database than the one under test, which this case's own
/// `DROP OWNED BY` (scoped to its own clone) cannot reach - without
/// abandoning the roles minted after it: the loop that drops them must
/// attempt every one, not stop at the first failure.
#[compio::test]
async fn an_undroppable_role_does_not_abandon_the_roles_minted_after_it() {
    // `anchor` only hosts the schema that makes the first role undroppable
    // and cleans it up afterward; it mints nothing of its own.
    Database::run(async |anchor| {
        let undroppable = RefCell::new(String::new());
        let droppable = RefCell::new(String::new());
        let outcome = AssertUnwindSafe(Database::run(async |database| {
            let first = database.mint_role("undroppable");
            let second = database.mint_role("droppable");
            *undroppable.borrow_mut() = first.clone();
            *droppable.borrow_mut() = second.clone();
            database
                .admin
                .batch_execute(&format!(
                    "CREATE ROLE \"{first}\" NOLOGIN; CREATE ROLE \"{second}\" NOLOGIN"
                ))
                .await
                .unwrap();
            // `first` owns a schema on `anchor`'s clone, a database other
            // than this case's own, so this case's teardown can never reach
            // it through its own `DROP OWNED BY`.
            anchor
                .admin
                .batch_execute(&format!(
                    "CREATE SCHEMA \"anchor_{first}\" AUTHORIZATION \"{first}\""
                ))
                .await
                .unwrap();
        }))
        .catch_unwind()
        .await;

        let undroppable = undroppable.into_inner();
        let droppable = droppable.into_inner();

        let panic = outcome.expect_err("an undroppable role must fail the case's teardown");
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| panic!("the teardown panic did not carry a formatted message"));
        assert!(
            message.contains(&undroppable),
            "the teardown failure must name the role that could not be dropped: {message}"
        );
        assert!(
            !message.contains(&droppable),
            "the teardown failure must not blame the role that DID drop: {message}"
        );

        let remaining_droppable = anchor
            .admin
            .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&droppable])
            .await
            .unwrap();
        assert!(
            remaining_droppable.is_empty(),
            "a role minted after an undroppable one must still be dropped"
        );

        // Clean up what this case made undroppable: the schema, then the role
        // it was authorized to, which teardown above could not remove.
        anchor
            .admin
            .batch_execute(&format!("DROP SCHEMA \"anchor_{undroppable}\" CASCADE"))
            .await
            .unwrap();
        anchor
            .admin
            .batch_execute(&format!("DROP ROLE IF EXISTS \"{undroppable}\""))
            .await
            .unwrap();
    })
    .await;
}

/// A role minted on the shared server authenticates on any database of it -
/// roles are cluster-global - but a sibling case's clone never granted it
/// `CONNECT`, so a case cannot read through another case's role even though
/// the role itself resolves everywhere.
#[compio::test]
async fn a_cases_role_cannot_connect_to_another_cases_database() {
    Database::run(async |first| {
        let role = first.mint_role("fixture_role");
        first
            .admin
            .batch_execute(&format!("CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}'"))
            .await
            .unwrap();
        Database::run(async |second| {
            // The clone - not the role - is what must refuse: revoke on the
            // database the crossed connection targets, not the one that
            // minted the role.
            second
                .admin
                .batch_execute(&format!(
                    "REVOKE ALL ON DATABASE \"{database}\" FROM PUBLIC",
                    database = second.database_name()
                ))
                .await
                .unwrap();
            let crossed_url = second.url_as(&role);
            let crossed = compio_postgres::connect(crossed_url.as_str(), NoTls).await;
            let error = crossed.expect_err(
                "the first case's role must not reach the second case's database",
            );
            let db_error = error
                .as_db_error()
                .expect("the server must answer with a SQLSTATE, not a transport failure");
            assert_eq!(
                db_error.code().code(),
                "42501",
                "the refusal must be insufficient_privilege: {error}"
            );
        })
        .await;
    })
    .await;
}
