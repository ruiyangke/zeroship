//! The PostgreSQL server compio-postgres's suites and live benches dial.
//!
//! Every test process of a worktree that asks for the server joins one shared
//! server through `zeroship_shared_server`: the first process elects itself and
//! boots it, every other process joins the ready server. A container per test
//! would put a server per test process on the machine; one per worktree run is
//! paid once. Isolation inside the server is the tests' own: each names the
//! tables, slots and publications it creates after itself and its process.
//!
//! Nothing is configured from outside. The server is found through a lease
//! directory under the worktree's `target`, so there is no address to export and
//! no provisioned instance to find. Docker is the one prerequisite: a process
//! that cannot join the server fails every test that asked for it, and the
//! failure names the reason. There is no skip.
//!
//! THE SERVER DOES NOT OUTLIVE ITS LAST LEASE. A process holds its lease for as
//! long as it runs and the kernel releases it however the process ends; the
//! watchdog that is the container's first process removes the container once no
//! process has held the lease for the idle grace.
//!
//! THE SETTINGS ARE THE SUITE'S, and every one of them is a restart-only server
//! parameter, so it is on the command line rather than applied by a session:
//!
//! - `wal_level=logical`: the replication and pgoutput suites create logical
//!   slots, which PostgreSQL refuses below `logical`.
//! - `max_prepared_transactions`: PostgreSQL's default is zero, which makes
//!   `PREPARE TRANSACTION` unavailable and leaves pgoutput's `two_phase`
//!   protocol untestable. The two-phase suite exercises the real
//!   prepare/commit/rollback frames.
//! - `max_replication_slots` and `max_wal_senders`: both default to ten. The
//!   suites run concurrently against this one server and every test that
//!   decodes a stream holds a slot and a walsender at once; a slot is also not
//!   released the instant its stream closes. At the default a run fails with
//!   `all replication slots are in use` (53400) before its assertions run.
//! - `max_slot_wal_keep_size` bounds the WAL a slot left by a killed test can
//!   pin, so an abandoned slot cannot fill the container's disk.

use std::sync::OnceLock;

use zeroship_shared_server::{self as shared, Scope};

/// The PostgreSQL image the suite runs against. Its protocol claims were
/// measured on PostgreSQL 16; a server of another major answers some of them
/// differently, which is what [`POSTGRES_18`] is for.
pub const IMAGE: &str = "postgres:16";

/// The second major the suite is crossed against, with the same settings.
pub const POSTGRES_18: &str = "postgres:18";

/// The superuser's password. The server listens on a loopback-mapped port for
/// the life of one worktree run, so this is a fixture value, not a credential.
const PASSWORD: &str = "compio-postgres-fixture";

/// The database every suite connects to.
const DATABASE: &str = "compio_postgres";

/// The port PostgreSQL listens on inside the container.
const POSTGRES_PORT: u16 = 5432;

/// The scope kind each shared server is filed under in the worktree's `target`.
const KIND: &str = "compio-postgres";
const KIND_18: &str = "compio-postgres-18";

/// The server, leased by the process that holds it.
#[derive(Debug)]
pub struct Server {
    lease: shared::Lease,
}

impl Server {
    /// Join the server of `image` that `scope` names, booting it if this
    /// process is elected.
    ///
    /// # Errors
    /// When the image cannot be built or the server cannot be booted or joined.
    pub fn join(scope: &Scope, image: &str) -> Result<Self, String> {
        let lease = shared::join(scope, &spec(image)?, |_| Ok(()))?;
        Ok(Self { lease })
    }

    /// The suite's database as the superuser, `postgres`, in URL form.
    ///
    /// URL form rather than `key=value`: callers append their own parameters
    /// and choose `?` or `&` by looking for a `?`.
    #[must_use]
    pub fn url(&self) -> String {
        format!(
            "postgres://postgres:{PASSWORD}@127.0.0.1:{}/{DATABASE}",
            self.lease.port
        )
    }

    /// The host port mapped to the server's PostgreSQL port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.lease.port
    }

    /// The Docker id of the container the server runs in.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.lease.container_id
    }
}

/// The worktree's shared PostgreSQL 16 server, joined on first use.
///
/// # Panics
/// When the server could not be joined - most often because Docker is not
/// available. The first failure is kept, so every later caller in the process
/// fails with the same reason instead of retrying a boot that cannot succeed.
pub fn server() -> &'static Server {
    static SERVER: OnceLock<Result<Server, String>> = OnceLock::new();
    joined(
        SERVER.get_or_init(|| Server::join(&Scope::worktree(KIND), IMAGE)),
        IMAGE,
    )
}

/// The worktree's shared PostgreSQL 18 server, with the same settings and
/// database as [`server`], joined on first use.
///
/// # Panics
/// As [`server`].
pub fn server_on_postgres_18() -> &'static Server {
    static SERVER: OnceLock<Result<Server, String>> = OnceLock::new();
    joined(
        SERVER.get_or_init(|| Server::join(&Scope::worktree(KIND_18), POSTGRES_18)),
        POSTGRES_18,
    )
}

fn joined<'a>(outcome: &'a Result<Server, String>, image: &str) -> &'a Server {
    match outcome {
        Ok(server) => server,
        Err(reason) => panic!(
            "compio-postgres's live tests run against a {image} server every test process of \
             the worktree shares, started in Docker by compio_postgres_testkit::server, and it \
             could not be joined: {reason}"
        ),
    }
}

/// The recipe the shared server runs under, its identity keyed to every input
/// that changes what a ready server holds.
fn spec(base: &str) -> Result<shared::Spec, String> {
    let image = shared::image::with_watchdog(base)?;
    let environment = vec![
        ("POSTGRES_PASSWORD".to_owned(), PASSWORD.to_owned()),
        ("POSTGRES_DB".to_owned(), DATABASE.to_owned()),
    ];
    let args: Vec<String> = [
        "docker-entrypoint.sh",
        "postgres",
        "-c",
        "wal_level=logical",
        "-c",
        "max_prepared_transactions=10",
        "-c",
        "max_replication_slots=128",
        "-c",
        "max_wal_senders=128",
        "-c",
        "max_slot_wal_keep_size=8GB",
    ]
    .iter()
    .map(|argument| (*argument).to_owned())
    .collect();
    let list = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
    let ready = shared::Readiness {
        log_marker: "PostgreSQL init process complete".to_owned(),
        probe: list(&[
            "pg_isready",
            "-U",
            "postgres",
            "-h",
            "127.0.0.1",
            "-p",
            "5432",
        ]),
        answer: list(&["psql", "-U", "postgres", "-d", DATABASE, "-tAc", "SELECT 1"]),
    };
    let mut parts: Vec<Vec<u8>> = vec![image.as_bytes().to_vec()];
    for argument in &args {
        parts.push(argument.as_bytes().to_vec());
    }
    for (key, value) in &environment {
        parts.push(format!("{key}={value}").into_bytes());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    Ok(shared::Spec {
        inputs: shared::digest(&refs),
        image,
        environment,
        ports: vec![shared::Port {
            container: POSTGRES_PORT,
            host: shared::HostPort::Assigned,
        }],
        watchdog: shared::image::WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Image,
        args,
        ready,
    })
}
