//! The PostgreSQL server the live suites share.
//!
//! Every test process of a worktree that asks for the server joins one shared
//! server through [`zeroship_shared_server`]: the first process elects itself
//! and boots it, every other process joins the ready server. A container per
//! test would put dozens of servers on the machine at once; one per worktree run
//! is paid once. Isolation inside the server is the caller's:
//! [`super::pg_database`] hands each PostgreSQL test a database of its own.
//!
//! Nothing is configured from outside. The server is found through a lease
//! directory under the worktree's `target`, so there is no address to export and
//! no provisioned instance to find. Docker is the one prerequisite: a process
//! that cannot join the server fails every test that asked for one, and the
//! failure names the server and the reason. There is no skip.
//!
//! THE SERVER DOES NOT OUTLIVE ITS LAST LEASE. The process holds its lease for
//! as long as it runs, and the kernel releases it however the process ends; the
//! watchdog that is the container's first process removes the container once no
//! process has held the lease for the idle grace. `pg_engine::owned_server`
//! measures that against real child processes, one that exits and one killed
//! while the server starts.

use std::sync::OnceLock;

use zeroship_shared_server::{self as shared, Scope};

/// The password of the superuser on the shared server. The server listens on a
/// loopback-mapped port for the life of one worktree run, so this is a fixture
/// value, not a credential.
const PASSWORD: &str = "zeroship-migrate-fixture";

/// The PostgreSQL image. The suites target PostgreSQL 18: database-generated UUIDv7
/// exists from 18 on, and the catalog shapes the drift suites pin were read from it.
const POSTGRES_IMAGE: zeroship_shared_server::images::Image =
    zeroship_shared_server::images::POSTGRES_18;
const POSTGRES_PORT: u16 = 5432;
/// The database the admin connection uses to create and drop per-test databases.
const POSTGRES_ADMIN_DATABASE: &str = "postgres";

/// The scope kind the shared server is filed under in the worktree's `target`.
const KIND: &str = "migrate-postgres";

/// The shared server, leased by the process that holds it.
#[derive(Debug)]
pub struct SharedServer {
    lease: shared::Lease,
}

impl SharedServer {
    /// Join the server `scope` names, booting it if this process is elected.
    ///
    /// [`postgres`] joins the worktree's scope; a lifetime measurement joins a
    /// throwaway one.
    ///
    /// # Errors
    /// When the image cannot be built or the server cannot be booted or joined.
    pub fn join(scope: &Scope) -> Result<Self, String> {
        let lease = shared::join(scope, &spec()?, |_| Ok(()))?;
        Ok(Self { lease })
    }

    /// The Docker id of the container, for a test that watches its lifetime.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.lease.container_id
    }

    /// A PostgreSQL DSN for `database` on this server, as its superuser.
    #[must_use]
    pub fn postgres_url(&self, database: &str) -> String {
        let mut url = url::Url::parse(&format!("postgresql://postgres@127.0.0.1/{database}"))
            .expect("the fixture URL template parses");
        url.set_password(Some(PASSWORD))
            .expect("the fixture URL accepts a password");
        url.set_port(Some(self.lease.port))
            .expect("the fixture URL accepts a port");
        url.into()
    }

    /// The PostgreSQL DSN of the admin database.
    #[must_use]
    pub fn postgres_admin_url(&self) -> String {
        self.postgres_url(POSTGRES_ADMIN_DATABASE)
    }
}

/// The recipe the shared server runs under, its identity keyed to every input
/// that changes what a ready server holds.
fn spec() -> Result<shared::Spec, String> {
    let image = zeroship_shared_server::image::with_watchdog(&POSTGRES_IMAGE.reference())?;
    let environment = vec![("POSTGRES_PASSWORD".to_owned(), PASSWORD.to_owned())];
    // Every test of a run reaches this one server, each with its own sessions, so
    // the stock connection ceiling is too low for a run on every core.
    // Durability is not under test.
    let args: Vec<String> = [
        "docker-entrypoint.sh",
        "postgres",
        "-c",
        "max_connections=400",
        "-c",
        "fsync=off",
    ]
    .iter()
    .map(|argument| (*argument).to_owned())
    .collect();
    let list = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
    let ready = shared::Readiness {
        log_marker: "PostgreSQL init process complete".to_owned(),
        probe: list(&["pg_isready", "-U", "postgres", "-h", "127.0.0.1", "-p", "5432"]),
        answer: list(&[
            "psql",
            "-U",
            "postgres",
            "-d",
            POSTGRES_ADMIN_DATABASE,
            "-tAc",
            "SELECT 1",
        ]),
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
        watchdog: zeroship_shared_server::image::WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Image,
        args,
        ready,
    })
}

/// How long a server a case owns outlives the case's lease.
const PRIVATE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// A server of the calling case's own, booted from the same recipe.
///
/// PostgreSQL roles are cluster-global, and the catalog snapshot the drift and
/// fold suites compare reads every role on the server. A case that creates,
/// alters or drops roles on the shared server would therefore change what every
/// other case's snapshot reads, between the two snapshots that case compares.
/// Such a case takes a server of its own: its lease is the server's only lease,
/// so once the handle drops, or the process holding it dies, the watchdog
/// removes the server.
///
/// # Panics
/// When the server could not be booted.
pub fn private() -> SharedServer {
    SharedServer::join(&Scope::private(KIND, PRIVATE_GRACE)).unwrap_or_else(|reason| {
        panic!(
            "zeroship-migrate's role-writing PostgreSQL tests run against a {POSTGRES_IMAGE} \
             server of their own, and it could not be booted: {reason}"
        )
    })
}

/// The worktree's shared PostgreSQL server, joined on first use.
///
/// # Panics
/// When the server could not be joined. The first failure is kept, so every later
/// caller fails with the same reason instead of retrying a boot that cannot succeed.
pub fn postgres() -> &'static SharedServer {
    static SERVER: OnceLock<Result<SharedServer, String>> = OnceLock::new();
    match SERVER.get_or_init(|| SharedServer::join(&Scope::worktree(KIND))) {
        Ok(server) => server,
        Err(reason) => panic!(
            "zeroship-migrate's live PostgreSQL tests run against a {POSTGRES_IMAGE} server \
             every test process of the worktree shares, and it could not be joined: {reason}"
        ),
    }
}
