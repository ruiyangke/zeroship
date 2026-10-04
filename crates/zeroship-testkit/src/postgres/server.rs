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

use crate::postgres::image;
use crate::shared::{self, Scope};

/// The pristine database every case database is cloned from.
const TEMPLATE: &str = "zeroship_bare_template";

/// The superuser's password. The server listens on a loopback-mapped port for
/// the life of one worktree run, so this is a fixture value, not a credential.
const PASSWORD: &str = "fixture";

/// The database the readiness probe connects to. It always exists and stays
/// connectable, unlike the sealed template a clone is taken from.
const PROBE_DATABASE: &str = "postgres";

/// A database cloned from the worktree's bare server, owned by one case.
#[derive(Debug)]
pub struct Postgres {
    name: String,
    port: u16,
    container_id: String,
    /// The lease of a server this handle joined on its own; `None` when the
    /// process-wide lease of the worktree's server holds it.
    _lease: Option<shared::Lease>,
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

    /// Join the bare server `scope` names and take a database of this case's
    /// own, holding that server's lease for as long as the handle lives.
    ///
    /// [`Postgres::start`] joins the worktree's scope; a lifetime measurement
    /// joins a throwaway one.
    ///
    /// # Errors
    /// When the server cannot be booted or joined, or the database cannot be
    /// cloned.
    pub fn start_in(scope: &Scope) -> Result<Self, String> {
        let lease = shared::join(scope, &spec()?, boot_bare)?;
        let name = clone_database(&lease.container_id)?;
        Ok(Self {
            name,
            port: lease.port,
            container_id: lease.container_id.clone(),
            _lease: Some(lease),
        })
    }

    /// The case database's URL as its superuser, `postgres`.
    #[must_use]
    pub fn url(&self) -> String {
        format!(
            "postgresql://postgres:{PASSWORD}@127.0.0.1:{}/{}",
            self.port, self.name
        )
    }

    /// The case database's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The host port mapped to the shared server's PostgreSQL port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The superuser a case connects as.
    #[must_use]
    pub fn user(&self) -> &'static str {
        "postgres"
    }

    /// The superuser's password, a fixture value on a loopback-mapped port.
    #[must_use]
    pub fn password(&self) -> &'static str {
        PASSWORD
    }

    /// The Docker id of the shared server the case database lives on.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    fn try_start() -> Result<Self, String> {
        let lease = bare_server()?;
        let name = clone_database(&lease.container_id)?;
        Ok(Self {
            name,
            port: lease.port,
            container_id: lease.container_id.clone(),
            _lease: None,
        })
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        let _ = shared::psql(
            &self.container_id,
            PROBE_DATABASE,
            &format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name),
        );
    }
}

/// Clone the pristine template under a minted name.
fn clone_database(container_id: &str) -> Result<String, String> {
    let name = DatabaseId::mint().as_str().to_owned();
    shared::psql(
        container_id,
        PROBE_DATABASE,
        &format!("CREATE DATABASE \"{name}\" TEMPLATE \"{TEMPLATE}\""),
    )?;
    Ok(name)
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
        ("POSTGRES_PASSWORD".to_owned(), PASSWORD.to_owned()),
        ("POSTGRES_DB".to_owned(), TEMPLATE.to_owned()),
    ];
    let postgres_args: Vec<String> = [
        "-c",
        "max_connections=500",
        "-c",
        "fsync=off",
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
