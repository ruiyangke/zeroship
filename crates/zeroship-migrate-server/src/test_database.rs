//! The PostgreSQL databases the migration server's unit tests run in.
//!
//! Each case takes a database of its own on the bare server every test process
//! of a worktree shares ([`zeroship_testkit::postgres::server`]): cloned from the
//! server's pristine template when the case asks, dropped when the case's
//! [`TestDatabase`] drops. The roles a case converges are named from the ids it
//! mints, so two cases on the one server never reach each other's. Nothing here
//! holds a container: the server belongs to the shared mechanism, whose watchdog
//! removes it once the run's last lease is gone, which [`server_lifetime`]
//! measures.

use std::ops::{Deref, DerefMut};

use compio_postgres::{Client, NoTls};
use zeroship_testkit::postgres::server::Postgres;

/// One case's connection to a database of its own.
///
/// The client is declared before the database so it closes first; the database
/// is then dropped `WITH (FORCE)`, which ends any session a case left open.
pub(crate) struct TestDatabase {
    client: Client,
    _database: Postgres,
}

impl Deref for TestDatabase {
    type Target = Client;

    fn deref(&self) -> &Client {
        &self.client
    }
}

impl DerefMut for TestDatabase {
    fn deref_mut(&mut self) -> &mut Client {
        &mut self.client
    }
}

/// A database of this case's own on the shared bare server, and a superuser
/// connection to it.
pub(crate) async fn connect() -> TestDatabase {
    let database = Postgres::start();
    let (client, connection) = compio_postgres::connect(&database.url(), NoTls)
        .await
        .expect("connect to the migration-server test database");
    // The connection ends when the case drops its database `WITH (FORCE)`, so
    // its closing error is the teardown, not a failure.
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    TestDatabase {
        client,
        _database: database,
    }
}

/// The unit tests' server is removed once the last process holding it has
/// ended: after a normal exit, and after a `SIGKILL` while it is still
/// starting. Both run [`server_lifetime::child_joins_the_unit_test_server`]
/// alone in a child process with a throwaway scope; see
/// `zeroship_testkit::lifetime`.
mod server_lifetime {
    use zeroship_testkit::lifetime;
    use zeroship_testkit::postgres::server::Postgres;

    /// The child test, by its full path in this binary.
    const CHILD_TEST: &str = "test_database::server_lifetime::child_joins_the_unit_test_server";

    #[test]
    #[ignore = "spawned by a lifetime measurement as a child process, with its scope on stdin"]
    fn child_joins_the_unit_test_server() {
        let database = Postgres::start_in(&lifetime::child_scope())
            .expect("join the unit tests' server at the throwaway scope");
        assert!(
            database.url().ends_with(database.name()),
            "the case's DSN names its own database: {}",
            database.url()
        );
        lifetime::report_container(database.container_id());
    }

    #[test]
    fn the_unit_test_server_is_removed_when_its_process_ends() {
        lifetime::assert_removed_after_the_child_exits(CHILD_TEST);
    }

    #[test]
    fn the_unit_test_server_is_removed_when_its_process_is_killed_while_starting() {
        lifetime::assert_removed_after_a_kill_during_startup(CHILD_TEST);
    }
}
