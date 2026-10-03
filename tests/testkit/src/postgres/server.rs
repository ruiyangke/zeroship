//! The bare PostgreSQL server every data test process of a worktree shares.
//!
//! [`Postgres::start`] joins the one bare server a worktree boots through
//! [`crate::shared`] and hands the caller a database cloned from the server's
//! pristine template. The first process elects itself, installs the template's
//! extensions, seals the template and records the boot; every other process
//! joins the ready server. Each case works in its own clone, so it may drop an
//! extension, create a fixed table or converge a cluster role without reaching
//! another case; the server underneath is shared, so a run pays for one
//! container rather than one per test. The clone is removed when the handle
//! drops.

use std::sync::OnceLock;

use zeroship_id::DatabaseId;

use crate::docker::DockerCli;
use crate::postgres::image;
use crate::shared::{self, Scope};

/// The pristine database every case database is cloned from.
const TEMPLATE: &str = "zeroship_bare_template";

/// The database the readiness probe connects to. It always exists and stays
/// connectable, unlike the sealed template a clone is taken from.
const PROBE_DATABASE: &str = "postgres";

/// A database cloned from the worktree's bare server, owned by one case.
pub struct Postgres {
    name: String,
    port: u16,
    container_id: String,
}

impl Postgres {
    /// Join the bare server and take a database of this case's own.
    ///
    /// # Panics
    /// When Docker or PostgreSQL is unavailable.
    #[must_use]
    pub fn start() -> Self {
        Self::try_start().expect("data tests require Docker and PostgreSQL")
    }

    /// The case database's URL as its superuser, `postgres`.
    #[must_use]
    pub fn url(&self) -> String {
        format!(
            "postgresql://postgres:fixture@127.0.0.1:{}/{}",
            self.port, self.name
        )
    }

    /// The Docker id of the shared server the case database lives on.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    fn try_start() -> Result<Self, String> {
        let lease = bare_server()?;
        let name = DatabaseId::mint().as_str().to_owned();
        shared::psql(
            &DockerCli::system(),
            &lease.container_id,
            PROBE_DATABASE,
            &format!("CREATE DATABASE \"{name}\" TEMPLATE \"{TEMPLATE}\""),
        )?;
        Ok(Self {
            name,
            port: lease.port,
            container_id: lease.container_id.clone(),
        })
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        let _ = shared::psql(
            &DockerCli::system(),
            &self.container_id,
            PROBE_DATABASE,
            &format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name),
        );
    }
}

/// Join the worktree's bare server and hold its lease for this process's life.
///
/// A test area that runs many test processes calls this once before them, so the
/// first boot is paid once and the watchdog does not tear the server down in a
/// gap between processes.
///
/// # Panics
/// When the shared server cannot be booted or joined.
#[must_use]
pub fn warm() -> &'static shared::Lease {
    bare_server().expect("the shared bare server must boot or join")
}

/// Join the worktree's bare server, booting it if this process is elected.
fn bare_server() -> Result<&'static shared::Lease, String> {
    static SERVER: OnceLock<Result<shared::Lease, String>> = OnceLock::new();
    SERVER
        .get_or_init(|| {
            let spec = spec()?;
            shared::join(&Scope::worktree("bare"), &spec, boot_bare)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Install the extensions a data case may use, then seal the pristine template
/// so every case database can be cloned from it.
fn boot_bare(boot: &shared::Boot) -> Result<(), String> {
    boot.psql(TEMPLATE, include_str!("extensions.sql"))?;
    boot.psql(
        PROBE_DATABASE,
        &format!("ALTER DATABASE \"{TEMPLATE}\" IS_TEMPLATE true ALLOW_CONNECTIONS false"),
    )
}

/// The recipe the bare server runs under, its identity keyed to every input
/// that changes what a case database holds.
fn spec() -> Result<shared::Spec, String> {
    let _ = image::build().map_err(|error| {
        format!("could not build the test PostgreSQL image (run `pnpm build` first): {error}")
    })?;
    let image = image::reference();
    let environment = vec![
        ("POSTGRES_PASSWORD".to_owned(), "fixture".to_owned()),
        ("POSTGRES_DB".to_owned(), TEMPLATE.to_owned()),
    ];
    let postgres_args: Vec<String> = [
        "-c",
        "wal_level=logical",
        "-c",
        "max_replication_slots=128",
        "-c",
        "max_wal_senders=128",
        "-c",
        "max_slot_wal_keep_size=256MB",
    ]
    .iter()
    .map(|argument| (*argument).to_owned())
    .collect();
    let args = super::server_args(&postgres_args);
    let inputs = inputs(&image, &environment, &args);
    Ok(shared::Spec {
        inputs,
        image,
        environment,
        ports: vec![super::port()],
        watchdog: super::WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Image,
        args,
        ready: super::readiness(PROBE_DATABASE),
    })
}

/// The 12-hex identity of a bare server built from the image, environment and
/// command arguments, plus the extensions it installs and the template it
/// seals.
fn inputs(image: &str, environment: &[(String, String)], args: &[String]) -> String {
    let mut parts: Vec<Vec<u8>> = vec![
        image.as_bytes().to_vec(),
        include_bytes!("extensions.sql").to_vec(),
        TEMPLATE.as_bytes().to_vec(),
        PROBE_DATABASE.as_bytes().to_vec(),
    ];
    for argument in args {
        parts.push(argument.as_bytes().to_vec());
    }
    for (key, value) in environment {
        parts.push(format!("{key}={value}").into_bytes());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    shared::digest(&refs)
}
