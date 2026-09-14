use std::sync::OnceLock;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

mod migrations;

pub(crate) struct Postgres {
    _container: Container<GenericImage>,
    url: String,
}

impl Postgres {
    fn migrated() -> Self {
        let container = GenericImage::new("postgres", "17")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "fixture")
            .with_env_var("POSTGRES_DB", "control_tests")
            .with_cmd(["postgres", "-c", "wal_level=logical", "-c", "fsync=off"])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .expect("control tests require Docker and PostgreSQL");
        let host = container.get_host().expect("database host");
        let port = container.get_host_port_ipv4(5432).expect("database port");
        let url = format!("postgresql://postgres:fixture@{host}:{port}/control_tests");
        migrations::apply(&url);
        Self {
            _container: container,
            url,
        }
    }

    fn url(&self) -> &str {
        &self.url
    }
}

static DATABASE: OnceLock<Postgres> = OnceLock::new();

pub(crate) fn url() -> String {
    DATABASE.get_or_init(Postgres::migrated).url().to_owned()
}
