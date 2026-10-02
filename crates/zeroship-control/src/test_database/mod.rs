//! The migrated PostgreSQL server a control test binary owns.
//!
//! One server per test binary, started and migrated the first time a test asks for
//! it. It lives in a `static`, which libtest never drops, so it is started through
//! the shared [`container_reaper`]: a reaper spawned before the container exists
//! removes it once this process has ended, however the process ends.

use std::sync::OnceLock;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{GenericImage, ImageExt};

mod migrations;

/// Containers a test process owns, removed when that process ends. Included
/// from the shared testkit copy so the reaper exists once in the tree.
#[path = "../../../../tests/testkit/src/docker.rs"]
pub(crate) mod container_reaper;

use container_reaper::{start_owned, DockerCli, OwnedContainer, Ownership};

pub(crate) struct Postgres {
    owned: OwnedContainer,
    url: String,
}

impl Postgres {
    fn migrated() -> Self {
        let request = GenericImage::new("postgres", "17")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "fixture")
            .with_env_var("POSTGRES_DB", "control_tests")
            // The suite opens a connection per query (the registry has no pool) and
            // each case may hold side connections; the default `max_connections` is
            // exhausted by the parallel runner. Size the server for it. Do not bound
            // idle sessions: the live-case harness already fails a case that leaks a
            // connection, and a server-side kill would hide the leak rather than
            // surface it.
            .with_cmd([
                "postgres",
                "-c",
                "wal_level=logical",
                "-c",
                "fsync=off",
                "-c",
                "max_connections=500",
            ])
            .with_startup_timeout(Duration::from_secs(120));
        let owned = start_owned(&DockerCli::system(), &Ownership::mint(), request)
            .unwrap_or_else(|error| panic!("control tests require Docker and PostgreSQL: {error}"));
        let container = owned.container();
        let host = container.get_host().expect("database host");
        let port = container.get_host_port_ipv4(5432).expect("database port");
        let url = format!("postgresql://postgres:fixture@{host}:{port}/control_tests");
        migrations::apply(&url);
        Self { owned, url }
    }

    fn url(&self) -> &str {
        &self.url
    }
}

static DATABASE: OnceLock<Postgres> = OnceLock::new();

pub(crate) fn url() -> String {
    DATABASE.get_or_init(Postgres::migrated).url().to_owned()
}

/// The Docker id of this binary's server, starting it if no test has yet.
#[allow(
    dead_code,
    reason = "the lifetime tests in the live_db binary read it; other binaries do not"
)]
pub(crate) fn container_id() -> String {
    DATABASE
        .get_or_init(Postgres::migrated)
        .owned
        .container()
        .id()
        .to_owned()
}
