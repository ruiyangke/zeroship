//! The PostgreSQL server the migration server's unit tests share.
//!
//! One server per test binary, kept in a `static`, which libtest never drops, so it
//! is started through the shared reaper in [`zeroship_testkit::docker`]: a reaper
//! spawned before the container exists removes it once this process has ended.

use std::sync::OnceLock;
use std::time::Duration;

use compio_postgres::{Client, NoTls};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{GenericImage, ImageExt};

use zeroship_testkit::{start_owned, DockerCli, OwnedContainer, Ownership};

struct Postgres {
    owned: OwnedContainer,
    url: String,
}

impl Postgres {
    fn start() -> Self {
        let request = GenericImage::new("postgres", "17")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "fixture")
            .with_env_var("POSTGRES_DB", "migrate_server_unit_tests")
            .with_cmd(["postgres", "-c", "wal_level=logical", "-c", "fsync=off"])
            .with_startup_timeout(Duration::from_secs(120));
        let owned = start_owned(&DockerCli::system(), &Ownership::mint(), request).unwrap_or_else(
            |error| panic!("migration-server unit tests require Docker and PostgreSQL: {error}"),
        );
        let container = owned.container();
        let host = container.get_host().expect("database host");
        let port = container.get_host_port_ipv4(5432).expect("database port");
        let url = format!("postgresql://postgres:fixture@{host}:{port}/migrate_server_unit_tests");
        Self { owned, url }
    }
}

static POSTGRES: OnceLock<Postgres> = OnceLock::new();

pub(crate) fn url() -> &'static str {
    &POSTGRES.get_or_init(Postgres::start).url
}

pub(crate) async fn connect() -> Client {
    let (client, connection) = compio_postgres::connect(url(), NoTls)
        .await
        .expect("connect to the migration-server test database");
    compio::runtime::spawn(async move {
        connection
            .run()
            .await
            .expect("migration-server test database connection");
    })
    .detach();
    client
}

/// This binary's server is removed once the process that started it has ended -
/// after a normal exit, and after a SIGKILL while it is still starting. Both run
/// [`server_lifetime::the_unit_test_server_reports_its_container`] alone in a child
/// process; see `zeroship_testkit::lifetime`.
mod server_lifetime {
    use super::{Postgres, POSTGRES};
    use zeroship_testkit::lifetime;

    /// The child test, by its full path in this binary.
    const CHILD_TEST: &str =
        "test_database::server_lifetime::the_unit_test_server_reports_its_container";

    #[test]
    fn the_unit_test_server_reports_its_container() {
        lifetime::report_owner();
        let server = POSTGRES.get_or_init(Postgres::start);
        assert!(
            server.url.ends_with("/migrate_server_unit_tests"),
            "{}",
            server.url
        );
        lifetime::report_container(server.owned.container().id());
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
