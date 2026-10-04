//! The MySQL server every test process of a worktree shares.
//!
//! [`server()`] joins the one server a worktree boots through [`crate::shared`]:
//! the first process elects itself, the image is built, the container starts,
//! and every other process joins the ready server. A process holds its lease for
//! as long as it runs; the container's watchdog removes the server once no
//! process has held it for the idle grace.
//!
//! One server serves every case; isolation is the caller's. A case creates the
//! database it owns under a name it mints and drops it through its own guard, so
//! two processes on the shared server cannot see or drop each other's state.

use std::sync::OnceLock;

use crate::shared::{self, HostPort, Port, Readiness, Scope, Spec};

mod image;
pub use image::{build, reference as image_ref};

/// The port MySQL listens on in the image.
const MYSQL_PORT: u16 = 3306;

/// The watchdog baked into the image.
const WATCHDOG: &str = "/usr/local/bin/zeroship-watchdog";

/// The password of the server's `root` superuser. The server listens on a
/// loopback-mapped port for the life of one worktree, so this is a fixture
/// value, not a credential.
const ROOT_PASSWORD: &str = "fixture";

/// The database the entrypoint creates and the readiness probe connects to. It
/// always exists and is what a caller's DSN names until the caller selects a
/// database of its own.
const BOOTSTRAP_DATABASE: &str = "zeroship_testkit";

/// A shared MySQL server, leased by the process that holds it.
#[derive(Debug)]
pub struct Mysql {
    lease: shared::Lease,
}

impl Mysql {
    /// The host a MySQL client connects to.
    #[must_use]
    pub fn host(&self) -> &'static str {
        "127.0.0.1"
    }

    /// The host port mapped to MySQL's port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.lease.port
    }

    /// The Docker id of the shared server.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.lease.container_id
    }

    /// The DSN of `database` on the shared server as its `root` superuser.
    #[must_use]
    pub fn url(&self, database: &str) -> String {
        format!(
            "mysql://root:{ROOT_PASSWORD}@{}:{}/{database}",
            self.host(),
            self.port()
        )
    }

    /// The DSN of the bootstrap database, which always exists.
    #[must_use]
    pub fn admin_url(&self) -> String {
        self.url(BOOTSTRAP_DATABASE)
    }

    /// The superuser a case connects as.
    #[must_use]
    pub fn user(&self) -> &'static str {
        "root"
    }

    /// The superuser's password, a fixture value on a loopback-mapped port.
    #[must_use]
    pub fn password(&self) -> &'static str {
        ROOT_PASSWORD
    }

    /// A database of the calling case's own, created under a minted name and
    /// dropped when the returned handle drops.
    ///
    /// # Errors
    /// When the server refuses the `CREATE DATABASE`.
    pub fn case_database(&self) -> Result<CaseDatabase, String> {
        let name = zeroship_id::DatabaseId::mint().as_str().to_owned();
        self.statement(&format!("CREATE DATABASE `{name}`"))?;
        Ok(CaseDatabase {
            name,
            container_id: self.lease.container_id.clone(),
        })
    }

    /// Run one statement as `root` through the container's `mysql` client.
    fn statement(&self, sql: &str) -> Result<(), String> {
        statement(&self.lease.container_id, sql)
    }
}

/// A database one case owns on the shared server.
#[derive(Debug)]
pub struct CaseDatabase {
    name: String,
    container_id: String,
}

impl CaseDatabase {
    /// The database's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for CaseDatabase {
    fn drop(&mut self) {
        let _ = statement(
            &self.container_id,
            &format!("DROP DATABASE IF EXISTS `{}`", self.name),
        );
    }
}

/// Run `sql` as `root` through the `mysql` client inside `container_id`.
fn statement(container_id: &str, sql: &str) -> Result<(), String> {
    let password = format!("-p{ROOT_PASSWORD}");
    let output = shared::exec_in_container(
        container_id,
        &["mysql", "-h", "127.0.0.1", "-uroot", &password, "-e", sql],
    )?;
    if output.success() {
        return Ok(());
    }
    Err(format!(
        "mysql on {container_id} failed (exit {:?}): {}",
        output.exit_code,
        output.stderr_text().trim()
    ))
}

/// Join the worktree's shared server, booting it if this process is elected.
///
/// # Panics
/// When the image cannot be built, or the server cannot be booted or joined.
#[must_use]
pub fn server() -> &'static Mysql {
    static SERVER: OnceLock<Mysql> = OnceLock::new();
    SERVER.get_or_init(|| {
        let spec = spec().unwrap_or_else(|error| {
            panic!("the shared MySQL server recipe could not be built: {error}")
        });
        let lease = shared::join(&Scope::worktree("mysql"), &spec, |_| Ok(()))
            .unwrap_or_else(|error| panic!("the shared MySQL server could not start: {error}"));
        Mysql { lease }
    })
}

/// The recipe the shared server runs under, its identity keyed to every input
/// that changes what a ready server holds.
///
/// # Errors
/// When the image cannot be built.
pub fn spec() -> Result<Spec, String> {
    let _ = build().map_err(|error| format!("could not build the shared MySQL image: {error}"))?;
    let image = image_ref();
    let environment = vec![
        ("MYSQL_ROOT_PASSWORD".to_owned(), ROOT_PASSWORD.to_owned()),
        ("MYSQL_DATABASE".to_owned(), BOOTSTRAP_DATABASE.to_owned()),
    ];
    let args = vec!["docker-entrypoint.sh".to_owned(), "mysqld".to_owned()];
    let ready = readiness();
    let inputs = inputs(&image, &environment, &args, &ready);
    Ok(Spec {
        inputs,
        image,
        environment,
        ports: vec![Port {
            container: MYSQL_PORT,
            host: HostPort::Assigned,
        }],
        watchdog: WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Image,
        args,
        ready,
    })
}

/// How to tell MySQL is ready and still answers.
///
/// The entrypoint initialises the data directory behind a temporary server that
/// also logs `ready for connections`, so the readiness marker is the line only
/// the final server prints: it names the listening port. The probe answers as
/// soon as the final server listens; the answer probe connects to the bootstrap
/// database, so a server that lost it reads as unreachable rather than replaced.
fn readiness() -> Readiness {
    let credentials = || {
        vec![
            "-h".to_owned(),
            "127.0.0.1".to_owned(),
            "-P".to_owned(),
            MYSQL_PORT.to_string(),
            "-uroot".to_owned(),
            format!("-p{ROOT_PASSWORD}"),
        ]
    };
    let mut probe = vec!["mysqladmin".to_owned(), "ping".to_owned()];
    probe.extend(credentials());
    probe.push("--silent".to_owned());
    let mut answer = vec!["mysql".to_owned()];
    answer.extend(credentials());
    answer.extend([
        "-D".to_owned(),
        BOOTSTRAP_DATABASE.to_owned(),
        "-N".to_owned(),
        "-e".to_owned(),
        "SELECT 1".to_owned(),
    ]);
    Readiness {
        log_marker: format!("port: {MYSQL_PORT}  MySQL Community Server"),
        probe,
        answer,
    }
}

/// The 12-hex identity of a server built from the image, environment, server
/// command and readiness probe.
fn inputs(
    image: &str,
    environment: &[(String, String)],
    args: &[String],
    ready: &Readiness,
) -> String {
    let mut parts: Vec<Vec<u8>> = vec![image.as_bytes().to_vec(), MYSQL_PORT.to_string().into_bytes()];
    for argument in args {
        parts.push(argument.as_bytes().to_vec());
    }
    for (key, value) in environment {
        parts.push(format!("{key}={value}").into_bytes());
    }
    parts.push(ready.log_marker.as_bytes().to_vec());
    for argument in ready.probe.iter().chain(&ready.answer) {
        parts.push(argument.as_bytes().to_vec());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    shared::digest(&refs)
}
