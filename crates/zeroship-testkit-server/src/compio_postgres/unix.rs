//! The `PostgreSQL` server the Unix-socket suite connects to through a socket
//! file on this host.
//!
//! A shared server of the worktree like [`super::server`], with one difference
//! that is the point: it listens on a Unix socket in a directory this host can
//! reach. The lease directory is the one directory the container already shares
//! with the host, so the server is told to put its socket in a `socket`
//! directory inside it, which the container creates, open to the server's own
//! user, before the server starts.
//!
//! WHY THE PATH THE SUITE DIALS IS A LINK. A Unix socket address is a fixed
//! 108-byte `sockaddr_un.sun_path` on Linux, and the server appends
//! `/.s.PGSQL.<port>` to its directory. A lease directory sits under the
//! checkout's `target`, so a deep checkout leaves no room for the socket name.
//! The suite therefore dials a short symbolic link under the system temporary
//! directory, named for the lease directory it points at; the kernel resolves
//! the link while connecting, and `sun_path` only has to hold the link's path.

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::{self as shared, Scope};

/// The image the server runs.
const IMAGE: shared::images::Image = shared::images::POSTGRES_16;

/// The superuser's password, a fixture value. Socket connections are trusted
/// by the image's own `pg_hba.conf`, so the suite never sends it.
const PASSWORD: &str = "compio-postgres-unix-fixture";

/// The database the suite connects to.
const DATABASE: &str = "compio_postgres";

/// The server's socket directory inside the container: a directory inside the
/// bind-mounted lease directory, so the socket appears on the host.
const CONTAINER_SOCKET_DIR: &str = "/run/zeroship-testkit/socket";

/// The scope kind the shared server is filed under in the worktree's `target`.
const KIND: &str = "compio-postgres-unix";

/// The longest socket path `sun_path` holds, its terminating NUL excluded.
const SUN_PATH_MAX: usize = 107;

/// The server and the short path its socket is reached through.
#[derive(Debug)]
pub struct UnixServer {
    socket_dir: PathBuf,
    _lease: shared::Lease,
}

impl UnixServer {
    /// The directory holding the server's socket, as a client names it in
    /// `host=`.
    #[must_use]
    pub fn socket_dir(&self) -> &Path {
        &self.socket_dir
    }

    /// The database the suite connects to.
    #[must_use]
    pub const fn dbname(&self) -> &'static str {
        DATABASE
    }

    /// The role the suite connects as.
    #[must_use]
    pub const fn user(&self) -> &'static str {
        "postgres"
    }
}

/// The worktree's shared Unix-socket server, joined on first use.
///
/// # Panics
/// When the server could not be joined - most often because Docker is not
/// available - or its socket cannot be reached through a short path. The first
/// failure is kept, so every later caller fails with the same reason.
pub fn server() -> &'static UnixServer {
    static SERVER: OnceLock<Result<UnixServer, String>> = OnceLock::new();
    match SERVER.get_or_init(join) {
        Ok(server) => server,
        Err(reason) => panic!(
            "compio-postgres's Unix-socket suite runs against a {IMAGE} server every test \
             process of the worktree shares, started in Docker by \
             zeroship_testkit_server::compio_postgres::unix, and it could not be joined: {reason}"
        ),
    }
}

fn join() -> Result<UnixServer, String> {
    let lease = shared::join(&Scope::worktree(KIND), &spec()?, |_| Ok(()))?;
    let host_socket_dir = lease.dir.join("socket");
    if !host_socket_dir.is_dir() {
        return Err(format!(
            "the server is up but its socket directory {} is absent",
            host_socket_dir.display()
        ));
    }
    let socket_dir = short_link(&host_socket_dir)?;
    let socket = socket_dir.join(".s.PGSQL.5432");
    let length = socket.as_os_str().len();
    if length > SUN_PATH_MAX {
        return Err(format!(
            "the socket path {} is {length} bytes, past the {SUN_PATH_MAX} `sun_path` holds",
            socket.display()
        ));
    }
    Ok(UnixServer {
        socket_dir,
        _lease: lease,
    })
}

/// A short symbolic link to `target` under the system temporary directory,
/// named for `target`, created if absent.
///
/// Every process of the run computes the same name, so a link another process
/// created first is the one this process uses; a stale link from an earlier
/// run of the same scope points at the same directory.
fn short_link(target: &Path) -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    target.hash(&mut hasher);
    let link = std::env::temp_dir().join(format!("cpgsock-{:016x}", hasher.finish()));
    match symlink(target, &link) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "could not link {} to {}: {error}",
                link.display(),
                target.display()
            ));
        }
    }
    match std::fs::read_link(&link) {
        Ok(points_at) if points_at == target => Ok(link),
        Ok(points_at) => Err(format!(
            "{} already links to {}, not to {}",
            link.display(),
            points_at.display(),
            target.display()
        )),
        Err(error) => Err(format!(
            "could not read the link {}: {error}",
            link.display()
        )),
    }
}

/// The recipe the shared server runs under, its identity keyed to every input
/// that changes what a ready server holds.
fn spec() -> Result<shared::Spec, String> {
    let image = shared::image::with_watchdog(&IMAGE.reference())?;
    let environment = vec![
        ("POSTGRES_PASSWORD".to_owned(), PASSWORD.to_owned()),
        ("POSTGRES_DB".to_owned(), DATABASE.to_owned()),
    ];
    // The watchdog runs the command as root, which can create the socket
    // directory in the bind-mounted lease directory; the image's entrypoint
    // then starts the server as its own user, which the directory's mode lets
    // write the socket. The image's own socket directory stays first: its
    // entrypoint passes these settings to the server it initialises the
    // database under, and its init steps connect there.
    let start = format!(
        "mkdir -p {CONTAINER_SOCKET_DIR} && chmod 0777 {CONTAINER_SOCKET_DIR} && \
         exec docker-entrypoint.sh postgres \
         -c unix_socket_directories=/var/run/postgresql,{CONTAINER_SOCKET_DIR} \
         -c unix_socket_permissions=0777"
    );
    let args = vec!["sh".to_owned(), "-c".to_owned(), start];
    let list = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
    let ready = shared::Readiness {
        log_marker: "PostgreSQL init process complete".to_owned(),
        probe: list(&["pg_isready", "-U", "postgres", "-h", CONTAINER_SOCKET_DIR]),
        answer: list(&[
            "psql",
            "-U",
            "postgres",
            "-h",
            CONTAINER_SOCKET_DIR,
            "-d",
            DATABASE,
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
            container: 5432,
            host: shared::HostPort::Assigned,
        }],
        watchdog: shared::image::WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Image,
        args,
        ready,
    })
}
