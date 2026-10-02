//! An explicitly owned PostgreSQL server for a native test.
//!
//! Keep the fixture alive until its clients and service processes have stopped.
//! The container is started through the shared reaper ([`docker`]), so a process
//! killed while its clients are still connected leaves no server behind.

#[path = "image.rs"]
mod image;
#[path = "../docker.rs"]
mod docker;

use testcontainers::{core::IntoContainerPort, ImageExt};

use docker::{start_owned, DockerCli, OwnedContainer, Ownership};

pub struct Postgres {
    _container: OwnedContainer,
    url: String,
}

impl Postgres {
    pub fn start() -> Self {
        Self::try_start().expect("data tests require Docker and PostgreSQL")
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    fn try_start() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let container = start_owned(
            &DockerCli::system(),
            &Ownership::mint(),
            image::await_ready(image::build()?.with_exposed_port(5432.tcp()))
                .with_env_var("POSTGRES_PASSWORD", "fixture")
                .with_env_var("POSTGRES_DB", "zeroship_data_tests")
                .with_copy_to(
                    "/docker-entrypoint-initdb.d/extensions.sql",
                    include_bytes!("extensions.sql").to_vec(),
                )
                .with_cmd([
                    "postgres",
                    "-c",
                    "wal_level=logical",
                    "-c",
                    "max_replication_slots=128",
                    "-c",
                    "max_wal_senders=128",
                    "-c",
                    "max_slot_wal_keep_size=256MB",
                ]),
        )?;
        let mut url =
            url::Url::parse("postgresql://postgres:fixture@localhost/zeroship_data_tests")?;
        url.set_host(Some(&container.container().get_host()?.to_string()))?;
        url.set_port(Some(container.container().get_host_port_ipv4(5432)?))
            .map_err(|()| "cannot set mapped PostgreSQL port")?;
        Ok(Self {
            _container: container,
            url: url.into(),
        })
    }
}
