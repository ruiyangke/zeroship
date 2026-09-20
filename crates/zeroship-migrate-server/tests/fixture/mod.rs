use std::sync::OnceLock;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

mod migrations;
pub mod world;

pub struct Postgres {
    _container: Container<GenericImage>,
    url: String,
}

impl Postgres {
    pub fn start() -> Self {
        let container = GenericImage::new("postgres", "17")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "fixture")
            .with_env_var("POSTGRES_DB", "migrate_server_tests")
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .expect("migration-server tests require Docker and PostgreSQL");
        let host = container.get_host().expect("database host");
        let port = container.get_host_port_ipv4(5432).expect("database port");
        let url = format!("postgresql://postgres:fixture@{host}:{port}/migrate_server_tests");
        Self {
            _container: container,
            url,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn migrated() -> Self {
        let postgres = Self::start();
        migrations::apply(postgres.url());
        postgres
    }
}

static MIGRATED: OnceLock<Postgres> = OnceLock::new();

pub fn migrated_url() -> String {
    MIGRATED.get_or_init(Postgres::migrated).url().to_owned()
}
