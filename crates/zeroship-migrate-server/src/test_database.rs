use std::sync::OnceLock;
use std::time::Duration;

use compio_postgres::{Client, NoTls};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

struct Postgres {
    _container: Container<GenericImage>,
    url: String,
}

impl Postgres {
    fn start() -> Self {
        let container = GenericImage::new("postgres", "17")
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
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .expect("migration-server unit tests require Docker and PostgreSQL");
        let host = container.get_host().expect("database host");
        let port = container.get_host_port_ipv4(5432).expect("database port");
        let url = format!("postgresql://postgres:fixture@{host}:{port}/migrate_server_unit_tests");
        Self {
            _container: container,
            url,
        }
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
