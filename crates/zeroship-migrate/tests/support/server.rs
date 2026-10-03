//! The PostgreSQL and MySQL servers a live suite binary owns.
//!
//! Each binary starts a server the first time one of its tests asks for it, and every
//! test in the binary shares that server. A container per test would put dozens of
//! servers on the machine at once for a single `cargo test`; one per binary is the
//! cost of the binary, paid once. Isolation inside the server is the caller's:
//! [`super::pg_database`] hands each PostgreSQL test a database of its own, and the
//! MySQL tests create the throwaway databases they need, as they always have.
//!
//! Nothing is configured from outside. The server is a container this process
//! started, so there is no address to export, no provisioned instance to find, and no
//! way for a run to reach a server another run is using. Docker - the daemon and its
//! `docker` CLI - is the one prerequisite: a binary that cannot start its server fails
//! every test that asked for one, and the failure names the server and the reason.
//! There is no skip.
//!
//! THE SERVER DOES NOT OUTLIVE THE PROCESS. The server lives in a `static`, which
//! libtest never drops, so it is started through the shared reaper in
//! [`zeroship_testkit::docker`]: a reaper spawned before the
//! container exists removes it once this process has ended, and the start is refused
//! when the `docker` CLI that reaper runs cannot see the container. That module's doc
//! says what this covers and what it does not. `pg_engine/owned_server.rs` measures
//! three of its paths against real processes: a child test process that exits
//! normally, one SIGKILLed while its server is still starting, and a `docker` program
//! that cannot see the daemon.

use std::sync::OnceLock;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{ContainerRequest, GenericImage, ImageExt};

use zeroship_testkit::docker::{start_owned, DockerCli, OwnedContainer, Ownership};

/// The password of the superuser on both owned servers. The servers listen on a
/// loopback-mapped port for the life of one test process, so this is a fixture value,
/// not a credential.
const PASSWORD: &str = "zeroship-migrate-fixture";

/// The PostgreSQL image. The suites target PostgreSQL 18: database-generated UUIDv7
/// exists from 18 on, and the catalog shapes the drift suites pin were read from it.
const POSTGRES_IMAGE: (&str, &str) = ("postgres", "18");
const POSTGRES_PORT: u16 = 5432;
/// The database the admin connection uses to create and drop per-test databases.
const POSTGRES_ADMIN_DATABASE: &str = "postgres";

/// The MySQL image. The live MySQL suites read the catalog of the 8.4 line.
const MYSQL_IMAGE: (&str, &str) = ("mysql", "8.4");
const MYSQL_PORT: u16 = 3306;
/// The database the MySQL DSN names. Tests create their own databases beside it.
const MYSQL_DATABASE: &str = "zeroship_migrate_test";

/// How long a server gets to become ready, including an image pull on a cold machine.
const STARTUP: Duration = Duration::from_mins(5);

/// A server this process started, with the reaper that removes it.
#[derive(Debug)]
pub struct OwnedServer {
    owned: OwnedContainer,
    host: String,
    port: u16,
}

impl OwnedServer {
    /// The Docker id of the container, for a test that watches its lifetime.
    #[must_use]
    pub fn container_id(&self) -> &str {
        self.owned.container().id()
    }

    fn url(&self, scheme: &str, user: &str, database: &str) -> String {
        let mut url = url::Url::parse(&format!("{scheme}://{user}@localhost/{database}"))
            .expect("the fixture URL template parses");
        url.set_password(Some(PASSWORD))
            .expect("the fixture URL accepts a password");
        url.set_host(Some(&self.host))
            .expect("the container host is a valid URL host");
        url.set_port(Some(self.port))
            .expect("the fixture URL accepts a port");
        url.into()
    }

    /// A PostgreSQL DSN for `database` on this server, as its superuser.
    #[must_use]
    pub fn postgres_url(&self, database: &str) -> String {
        self.url("postgresql", "postgres", database)
    }

    /// The PostgreSQL DSN of the admin database.
    #[must_use]
    pub fn postgres_admin_url(&self) -> String {
        self.postgres_url(POSTGRES_ADMIN_DATABASE)
    }

    /// The MySQL DSN of the database this server was created with, as root.
    #[must_use]
    pub fn mysql_url(&self) -> String {
        self.url("mysql", "root", MYSQL_DATABASE)
    }
}

/// The container request the PostgreSQL server is started from.
pub fn postgres_request() -> ContainerRequest<GenericImage> {
    GenericImage::new(POSTGRES_IMAGE.0, POSTGRES_IMAGE.1)
        .with_exposed_port(POSTGRES_PORT.tcp())
        .with_wait_for(WaitFor::message_on_stdout(
            "PostgreSQL init process complete; ready for start up.",
        ))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        // Every test of the binary runs against this one server, each with its own
        // sessions, so the stock connection ceiling is too low for a binary running
        // on every core. Durability is not under test.
        .with_cmd(["postgres", "-c", "max_connections=400", "-c", "fsync=off"])
        .with_startup_timeout(STARTUP)
}

/// The container request the MySQL server is started from.
pub fn mysql_request() -> ContainerRequest<GenericImage> {
    GenericImage::new(MYSQL_IMAGE.0, MYSQL_IMAGE.1)
        .with_exposed_port(MYSQL_PORT.tcp())
        // The entrypoint runs a temporary server on port 0 to initialize the data
        // directory and logs `ready for connections` for it too. Only the final
        // server listens on 3306.
        .with_wait_for(WaitFor::message_on_either_std(
            "port: 3306  MySQL Community Server",
        ))
        .with_env_var("MYSQL_ROOT_PASSWORD", PASSWORD)
        .with_env_var("MYSQL_DATABASE", MYSQL_DATABASE)
        // Native AIO is a per-server kernel resource, and nextest runs one
        // process per test, so a suite that starts a server per test asks the
        // host for one server's worth of contexts per test. `io_setup` returns
        // EAGAIN once `fs.aio-max-nr` is reached and InnoDB aborts before the
        // server ever listens. The engine's asynchronous I/O is not under test
        // here, and simulated AIO is the documented fallback, so the fixture
        // takes the server off that shared budget.
        .with_cmd(["--innodb-use-native-aio=0"])
        .with_startup_timeout(STARTUP)
}

/// This binary's PostgreSQL server, started on first use.
///
/// # Panics
/// When the server could not be started. The first failure is kept, so every later
/// caller fails with the same reason instead of retrying a start that cannot succeed.
pub fn postgres() -> &'static OwnedServer {
    static SERVER: OnceLock<Result<OwnedServer, String>> = OnceLock::new();
    owned(
        &SERVER,
        "PostgreSQL",
        POSTGRES_IMAGE,
        POSTGRES_PORT,
        postgres_request,
    )
}

/// This binary's MySQL server, started on first use.
///
/// # Panics
/// When the server could not be started, with the same keep-the-first-failure rule
/// as [`postgres`].
pub fn mysql() -> &'static OwnedServer {
    static SERVER: OnceLock<Result<OwnedServer, String>> = OnceLock::new();
    owned(&SERVER, "MySQL", MYSQL_IMAGE, MYSQL_PORT, mysql_request)
}

fn owned(
    slot: &'static OnceLock<Result<OwnedServer, String>>,
    server: &str,
    (image, tag): (&str, &str),
    port: u16,
    request: impl FnOnce() -> ContainerRequest<GenericImage>,
) -> &'static OwnedServer {
    match slot.get_or_init(|| start(request(), port)) {
        Ok(owned) => owned,
        Err(reason) => panic!(
            "zeroship-migrate's live {server} tests run against a {image}:{tag} server this \
             test binary starts through Docker, and it could not be started: {reason}"
        ),
    }
}

fn start(request: ContainerRequest<GenericImage>, port: u16) -> Result<OwnedServer, String> {
    let owned = start_owned(&DockerCli::system(), &Ownership::mint(), request)?;
    let host = owned
        .container()
        .get_host()
        .map_err(|error| format!("the container host is unknown: {error}"))?
        .to_string();
    let port = owned
        .container()
        .get_host_port_ipv4(port)
        .map_err(|error| format!("port {port} is not mapped: {error}"))?;
    Ok(OwnedServer { owned, host, port })
}
