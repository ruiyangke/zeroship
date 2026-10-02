//! The PostgreSQL servers the migration server's integration tests run against.
//!
//! [`Postgres::start`] hands a test a server of its own; [`migrated_url`] shares one
//! migrated server across the binary, kept in a `static` that libtest never drops.
//! Both are started through the shared [`container_reaper`], whose reaper - spawned
//! before the container exists - removes the container once this process has ended,
//! so neither outlives the test process however it ends.

use std::sync::OnceLock;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{GenericImage, ImageExt};

mod migrations;
pub mod world;

/// Containers a test process owns, removed when that process ends.
#[path = "../../../../tests/fixtures/container_reaper.rs"]
pub mod container_reaper;

use container_reaper::{start_owned, DockerCli, OwnedContainer, Ownership};

pub struct Postgres {
    #[allow(
        dead_code,
        reason = "held for its Drop; read only by the binary that measures its lifetime"
    )]
    owned: OwnedContainer,
    url: String,
}

impl Postgres {
    pub fn start() -> Self {
        let request = GenericImage::new("postgres", "17")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "fixture")
            .with_env_var("POSTGRES_DB", "migrate_server_tests")
            .with_startup_timeout(Duration::from_secs(120));
        let owned = start_owned(&DockerCli::system(), &Ownership::mint(), request).unwrap_or_else(
            |error| panic!("migration-server tests require Docker and PostgreSQL: {error}"),
        );
        let container = owned.container();
        let host = container.get_host().expect("database host");
        let port = container.get_host_port_ipv4(5432).expect("database port");
        let url = format!("postgresql://postgres:fixture@{host}:{port}/migrate_server_tests");
        Self { owned, url }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The Docker id of this server's container.
    #[allow(
        dead_code,
        reason = "only the binary that measures the server's lifetime reads it"
    )]
    pub fn container_id(&self) -> &str {
        self.owned.container().id()
    }

    fn migrated() -> Self {
        let postgres = Self::start();
        migrations::apply(postgres.url());
        postgres
    }
}

static MIGRATED: OnceLock<Postgres> = OnceLock::new();

pub fn migrated_url() -> String {
    migrated().url().to_owned()
}

/// The binary's shared migrated server, started and migrated on first use.
#[allow(
    dead_code,
    reason = "binaries that only start servers of their own never reach it"
)]
pub fn migrated() -> &'static Postgres {
    MIGRATED.get_or_init(Postgres::migrated)
}
