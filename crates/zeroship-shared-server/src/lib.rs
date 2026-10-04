//! One server shared by every test process of a worktree.
//!
//! Every test binary of a run - nextest's process-per-test, several `cargo test`
//! binaries, an xtask-held run - joins the same server through this crate. The
//! processes elect one booter with an exclusive `flock` on a per-worktree
//! directory under `target/`, then every process holds a shared lock on the same
//! directory as its lease. A container-side watchdog watches that lease and
//! removes the container once no process has held it for the idle grace. The
//! server is whatever [`Spec`] describes: the platform database, a bare
//! PostgreSQL server, a Redpanda broker, or a driver suite's own server.
//!
//! The crate names no database driver, so a driver's own tests can lease a
//! server through it without linking a second copy of the driver under test.
//! [`image`] builds a stock image with the watchdog on top, and [`lifetime`]
//! measures that a fixture's container is removed once the process holding it
//! exits or is killed while it starts.
//!
//! There is no environment variable anywhere in the protocol: a process learns
//! the directory from the repository it was compiled in and the scope it is
//! given, and the container learns its lease from the bind-mounted directory.
//! Plain `cargo test` and nextest therefore take the same path.
//!
//! THE DAEMON API, NOT THE `docker` CLI. Every lifecycle call - create, start,
//! inspect, exec, logs, remove - goes through `testcontainers::bollard`, the
//! client testcontainers itself uses, so there is one Docker configuration and
//! no CLI that can disagree with it. The shared container is NOT started through
//! `testcontainers`' runner: that registers every container it creates with the
//! process watchdog, so a `SIGTERM` to the booting process would stop the server
//! the other leasers are still using. Instead the container is created with
//! `HostConfig::auto_remove` (the API form of `--rm`) and a label, and its
//! lifetime is owned by the in-container watchdog reading the flock.
//!
//! The directory holds three files:
//! - `session.lock`, whose shared lock is a process's lease and whose exclusive
//!   lock is what the watchdog probes to decide the server is idle;
//! - `boot.lock`, held exclusively by whichever process is booting, so the
//!   others wait rather than race;
//! - `state.json`, written by atomic rename, naming the container, its port and
//!   whether the boot is `booting`, `ready` or `failed`.

pub mod image;
pub mod lifetime;

use std::any::Any;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use sha2::{Digest, Sha256};
use testcontainers::bollard::{
    container::LogOutput,
    exec::StartExecResults,
    models::{
        ContainerCreateBody, ContainerStateStatusEnum, ExecConfig, ExecInspectResponse,
        HostConfig, PortBinding,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    },
    Docker,
};

/// The label naming the boot a container belongs to.
pub const SESSION_LABEL: &str = "zeroship.testkit.session";
/// The label naming the lease directory a container serves.
pub const DIR_LABEL: &str = "zeroship.testkit.dir";
/// The label naming the uid that started the container.
pub const UID_LABEL: &str = "zeroship.testkit.uid";
/// The label naming the inputs the server was built for.
pub const INPUTS_LABEL: &str = "zeroship.testkit.inputs";
/// The label naming the boot a container's watchdog removes state for.
pub const NONCE_LABEL: &str = "zeroship.testkit.nonce";

/// How long the lease may go unheld before the watchdog removes the container.
pub const DEFAULT_IDLE_GRACE: Duration = Duration::from_secs(60);

/// How long a process waits for the shared lease before giving up.
const LEASE_WAIT: Duration = Duration::from_mins(10);

/// How long a process waits for the boot lock before giving up.
const BOOT_LOCK_WAIT: Duration = Duration::from_mins(6);

/// How long a boot gets from container start to a migrated database.
const BOOT_TIMEOUT: Duration = Duration::from_mins(5);

/// How long one `exec` statement may run before it is killed.
const EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// How long past its own timeout an `exec` exchange may take before this side
/// stops waiting for the daemon: the in-container `timeout` sends `SIGTERM` at
/// the deadline and `SIGKILL` a second later, and the daemon then has to close
/// the attached stream.
const EXEC_SLACK: Duration = Duration::from_secs(5);

/// The exit status `timeout` reports when it stopped the command at its
/// deadline.
const TIMED_OUT: i64 = 124;

/// How long an `exec` probe may run before it counts as unanswered.
const ANSWER_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a server that is present may go without answering before this
/// caller reports it unreachable.
const ANSWER_DEADLINE: Duration = Duration::from_secs(15);

/// How long a boot waits for a removed container to disappear before creating
/// its replacement under the same name.
const REMOVE_WAIT: Duration = Duration::from_secs(30);

/// The mount point the lease directory is bound to in the container.
const CONTAINER_DIR: &str = "/run/zeroship-testkit";

/// The path of the lease inside the container.
const CONTAINER_LEASE: &str = "/run/zeroship-testkit/session.lock";

/// The name of the state file inside the lease directory.
const STATE_FILE: &str = "state.json";

/// Where a scope's server lives.
///
/// A worktree scope names its directory from the repository root and the inputs
/// its caller supplies; an explicit scope names it directly, which is what a
/// contract test uses to own a throwaway server.
#[derive(Debug, Clone)]
pub struct Scope {
    kind: String,
    explicit: Option<PathBuf>,
    idle_grace: Duration,
}

impl Scope {
    /// The worktree's shared scope for one kind of server.
    #[must_use]
    pub fn worktree(kind: &str) -> Self {
        Self::worktree_with_grace(kind, DEFAULT_IDLE_GRACE)
    }

    /// The worktree's shared scope for one kind of server, with `idle_grace`
    /// before an unheld server is removed.
    ///
    /// A server whose lease is held per test process rather than per test
    /// binary needs a grace that spans a run: the lease is unheld between the
    /// tests that use it, and a grace shorter than those gaps removes the
    /// server mid-run, so a later test boots a second one.
    #[must_use]
    pub fn worktree_with_grace(kind: &str, idle_grace: Duration) -> Self {
        Self {
            kind: kind.to_owned(),
            explicit: None,
            idle_grace,
        }
    }

    /// A scope at `dir`, with `idle_grace` before an unheld server is removed.
    ///
    /// # Panics
    /// When `idle_grace` is under one second: the watchdog counts whole seconds,
    /// so a sub-second grace would round to zero and tear the server down before
    /// its first lease could be taken.
    #[must_use]
    pub fn at(dir: impl Into<PathBuf>, idle_grace: Duration) -> Self {
        assert!(
            idle_grace >= Duration::from_secs(1),
            "an idle grace under one second would round to zero in the watchdog"
        );
        Self {
            kind: "explicit".to_owned(),
            explicit: Some(dir.into()),
            idle_grace,
        }
    }

    /// A scope no other caller names, under the worktree's `target`, with
    /// `idle_grace` before an unheld server is removed.
    ///
    /// For a case whose subject is cluster-global state the other cases on a
    /// shared server would observe - PostgreSQL roles, say - and that therefore
    /// needs a server of its own. The server is booted from the same recipe as
    /// the shared one; the case's lease is its only lease, so once the case
    /// releases it, or its process dies, the watchdog removes the server.
    ///
    /// # Panics
    /// As [`Scope::at`], when `idle_grace` is under one second.
    #[must_use]
    pub fn private(kind: &str, idle_grace: Duration) -> Self {
        Self::at(
            root()
                .join("target/zeroship-testkit")
                .join(format!("{kind}-private"))
                .join(mint_nonce()),
            idle_grace,
        )
    }

    /// The directory this scope's lease and state live in, once resolved.
    fn directory(&self, inputs: &str) -> Result<PathBuf, String> {
        let dir = match &self.explicit {
            Some(dir) => dir.clone(),
            None => root()
                .join("target/zeroship-testkit")
                .join(format!("{}-{inputs}", self.kind)),
        };
        fs::create_dir_all(&dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
        fs::canonicalize(&dir)
            .map_err(|error| format!("could not canonicalize {}: {error}", dir.display()))
    }

    /// How long the server may go unheld before the watchdog removes it.
    #[must_use]
    pub fn idle_grace(&self) -> Duration {
        self.idle_grace
    }
}

/// One published port in a [`Spec`].
#[derive(Debug, Clone)]
pub struct Port {
    /// The port inside the container.
    pub container: u16,
    /// How the host port is chosen.
    pub host: HostPort,
}

/// How a published port's host side is chosen.
#[derive(Debug, Clone, Copy)]
pub enum HostPort {
    /// The daemon assigns an ephemeral host port once the container is running.
    Assigned,
    /// The shared mechanism reserves a free host port before the container runs
    /// and substitutes it for [`HOST_PORT_TOKEN`] in [`Spec::args`]. A server
    /// that advertises its host port to clients, such as Redpanda, needs this;
    /// at most one port may reserve one.
    Reserved,
}

/// The argument token a [`HostPort::Reserved`] port is substituted for.
pub const HOST_PORT_TOKEN: &str = "{host_port}";

/// Whether the image's entrypoint runs, or the watchdog replaces it.
#[derive(Debug, Clone, Copy)]
pub enum Entrypoint {
    /// The image's own entrypoint runs and is handed the watchdog as its first
    /// argument; the PostgreSQL entrypoint execs a non-`postgres` first
    /// argument, which is what makes the watchdog PID 1.
    Image,
    /// The watchdog replaces the image's entrypoint. The image entrypoint must
    /// not run, because the Redpanda entrypoint would exec `rpk` with the
    /// watchdog as an argument.
    Watchdog,
}

/// How to tell a server is ready and still answers.
#[derive(Debug, Clone)]
pub struct Readiness {
    /// A substring the container's logs carry once the server has initialised.
    pub log_marker: String,
    /// Run in the container; exit 0 means the server is ready.
    pub probe: Vec<String>,
    /// Run in the container; exit 0 means a running server still answers.
    pub answer: Vec<String>,
}

/// How to run one kind of server.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The inputs the server is keyed to, part of the lease directory name.
    pub inputs: String,
    /// The image reference to run.
    pub image: String,
    /// The environment the container is started with.
    pub environment: Vec<(String, String)>,
    /// The ports to publish. The first is the server's primary port, exposed on
    /// [`Lease::port`] and [`Boot::port`].
    pub ports: Vec<Port>,
    /// The program inside the image that runs the server and watches the lease.
    pub watchdog: String,
    /// Whether the image's entrypoint runs, or the watchdog replaces it.
    pub entrypoint: Entrypoint,
    /// The server command, handed to the watchdog after the idle grace and
    /// nonce.
    pub args: Vec<String>,
    /// How to tell the server is ready and still answers.
    pub ready: Readiness,
}

/// One `exec`'s captured output.
#[derive(Debug)]
pub struct ExecOutput {
    /// Everything the command wrote to standard output.
    pub stdout: Vec<u8>,
    /// Everything the command wrote to standard error.
    pub stderr: Vec<u8>,
    /// The command's exit code, `None` when the daemon reported none.
    pub exit_code: Option<i64>,
}

impl ExecOutput {
    /// Whether the command exited zero.
    #[must_use]
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// The command's standard error as lossy UTF-8.
    #[must_use]
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// A boot's view of the container it started, handed to the boot closure.
#[derive(Debug)]
pub struct Boot {
    /// The container's full id.
    pub container_id: String,
    /// The container's deterministic name.
    pub name: String,
    /// The host port mapped to the server's primary port.
    pub port: u16,
    /// The nonce naming this boot.
    pub nonce: String,
}

impl Boot {
    /// Run one statement as the superuser through the container's `psql`.
    ///
    /// # Errors
    /// When the container cannot run `psql`, the statement fails, or the
    /// statement runs past [`EXEC_TIMEOUT`].
    pub fn psql(&self, database: &str, sql: &str) -> Result<(), String> {
        psql(&self.container_id, database, sql)
    }

    /// Run a program in the container and return its output.
    ///
    /// # Errors
    /// When the program cannot be run or runs past [`EXEC_TIMEOUT`].
    pub fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, String> {
        let mut command = vec![program.to_owned()];
        command.extend(args.iter().map(|argument| (*argument).to_owned()));
        exec_with_timeout(&self.container_id, command, EXEC_TIMEOUT)
    }
}

/// One process's hold on a shared server.
///
/// Dropping the lease releases the shared lock; once no process holds one, the
/// watchdog removes the container after the idle grace.
#[derive(Debug)]
pub struct Lease {
    /// The container's full id.
    pub container_id: String,
    /// The host port mapped to the server's port.
    pub port: u16,
    /// The nonce naming this boot.
    pub nonce: String,
    /// Whether this process started the server rather than joining one.
    pub booted: bool,
    /// The scope's canonical lease directory on this host, which the
    /// container has bind-mounted at `/run/zeroship-testkit`. A server that
    /// writes into that mount - a Unix socket, say - is reached here.
    pub dir: PathBuf,
    _session: Arc<File>,
}

/// What a ready state resolves to.
enum Ready {
    /// The state names a live server that answers: join it.
    Joined(Lease),
    /// No ready server the boot lock owner may keep: boot one.
    Absent,
    /// The state names a live server that does not answer. It is not replaced:
    /// the caller that found it keeps the connection error.
    Unreachable(String),
}

/// The daemon's view of a container named by a ready state.
#[derive(Debug)]
enum Presence {
    /// The daemon does not know the container.
    Gone,
    /// The container exists but is not running.
    Stopped,
    /// The container is running; carries its session label.
    Running { session: String },
}

/// Join the shared server `scope` describes, booting it through `boot` if no
/// ready server exists.
///
/// # Errors
/// When the lease cannot be taken, a recent failed boot is still inside its idle
/// grace, a live server named by the state does not answer, or booting the
/// server fails. The error is the recorded failure text.
pub fn join(
    scope: &Scope,
    spec: &Spec,
    boot: impl FnOnce(&Boot) -> Result<(), String>,
) -> Result<Lease, String> {
    let dir = scope.directory(&spec.inputs)?;
    let session = Arc::new(lock_shared(&dir.join("session.lock"))?);

    match ready(&dir, spec, &session)? {
        Ready::Joined(lease) => return Ok(lease),
        Ready::Unreachable(error) => return Err(error),
        Ready::Absent => {}
    }

    let boot_lock = open_lock(&dir.join("boot.lock"))?;
    lock_exclusive(&boot_lock, BOOT_LOCK_WAIT)?;

    match ready(&dir, spec, &session)? {
        Ready::Joined(lease) => return Ok(lease),
        Ready::Unreachable(error) => return Err(error),
        Ready::Absent => {}
    }

    if let Some(state) = read_state(&dir) {
        if state.get("status").and_then(|value| value.as_str()) == Some("failed") {
            let failed_at = state
                .get("failed_at")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            if now_millis().saturating_sub(failed_at) < scope.idle_grace.as_millis() as u64 {
                // A fresh failure is reported to every waiter rather than
                // retried. Only the booter stamps `failed_at`, so the failure
                // expires after one grace instead of being extended by each
                // reader.
                return Err(format!(
                    "the shared server failed to boot: {}",
                    state
                        .get("error")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown error")
                ));
            }
        }
    }

    boot_server(&dir, spec, scope, &session, boot)
}

/// Resolve a ready state to a server this process can lease.
///
/// A live container that does not answer is reported, never replaced: replacing
/// it would destroy a healthy server for every other process because one caller
/// could not reach it. Only a container the daemon reports gone or stopped, or
/// whose session label names a different boot, is treated as absent.
fn ready(dir: &Path, spec: &Spec, session: &Arc<File>) -> Result<Ready, String> {
    let Some(state) = read_state(dir) else {
        return Ok(Ready::Absent);
    };
    if state.get("status").and_then(|value| value.as_str()) != Some("ready") {
        return Ok(Ready::Absent);
    }
    let Some(container_id) = state.get("container_id").and_then(|value| value.as_str()) else {
        return Ok(Ready::Absent);
    };
    let Some(port) = state.get("port").and_then(serde_json::Value::as_u64) else {
        return Ok(Ready::Absent);
    };
    let Some(nonce) = state.get("nonce").and_then(|value| value.as_str()) else {
        return Ok(Ready::Absent);
    };

    match presence(container_id)? {
        Presence::Gone | Presence::Stopped => return Ok(Ready::Absent),
        Presence::Running { session: live } => {
            if live != nonce {
                return Ok(Ready::Absent);
            }
        }
    }

    if answers(container_id, &spec.ready.answer) {
        return Ok(Ready::Joined(Lease {
            container_id: container_id.to_owned(),
            port: port as u16,
            nonce: nonce.to_owned(),
            booted: false,
            dir: dir.to_path_buf(),
            _session: Arc::clone(session),
        }));
    }
    Ok(Ready::Unreachable(format!(
        "the shared server {container_id} is running under this boot's session but did not \
         answer within {ANSWER_DEADLINE:?}; not replacing a live server"
    )))
}

/// Whether the container answers through the recipe's answer command.
///
/// The probe stops as soon as the daemon says the container is gone, so a dead
/// server costs one `exec` rather than the whole probe budget.
fn answers(container_id: &str, answer: &[String]) -> bool {
    let deadline = Instant::now() + ANSWER_DEADLINE;
    loop {
        match exec_with_timeout(container_id, answer.to_vec(), ANSWER_PROBE_TIMEOUT) {
            Ok(output) => {
                if output.success() {
                    return true;
                }
                if output.stderr_text().to_ascii_lowercase().contains("no such container") {
                    return false;
                }
            }
            Err(error) => {
                if error.to_ascii_lowercase().contains("no such container") {
                    return false;
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What the daemon says about a container named by a ready state.
fn presence(container_id: &str) -> Result<Presence, String> {
    let docker = docker();
    match block_on(docker.inspect_container(container_id, None)) {
        Ok(inspect) => {
            let running = inspect
                .state
                .as_ref()
                .and_then(|state| state.status.as_ref())
                .is_some_and(|status| *status == ContainerStateStatusEnum::RUNNING);
            if running {
                let session = inspect
                    .config
                    .as_ref()
                    .and_then(|config| config.labels.as_ref())
                    .and_then(|labels| labels.get(SESSION_LABEL))
                    .cloned()
                    .unwrap_or_default();
                Ok(Presence::Running { session })
            } else {
                Ok(Presence::Stopped)
            }
        }
        Err(error) if is_absent(&error) => Ok(Presence::Gone),
        Err(error) => Err(format!("inspect {container_id} failed: {error}")),
    }
}

/// Whether a daemon error says the container does not exist.
///
/// Only the daemon's own "No such container" answer is absence. Any other
/// failure - an unreachable socket, a server error, a refused request - says
/// nothing about the container, so a caller that read it as "removed" would
/// pass a leak check over a daemon it could not ask.
fn is_absent(error: &testcontainers::bollard::errors::Error) -> bool {
    matches!(
        error,
        testcontainers::bollard::errors::Error::DockerResponseServerError { status_code: 404, message }
            if message.to_ascii_lowercase().contains("no such container")
    )
}

/// Boot a server in `dir` while holding the boot lock and the session lease.
fn boot_server(
    dir: &Path,
    spec: &Spec,
    scope: &Scope,
    session: &Arc<File>,
    boot: impl FnOnce(&Boot) -> Result<(), String>,
) -> Result<Lease, String> {
    let nonce = mint_nonce();
    let name = container_name(dir);

    write_state(dir, &state("booting", &nonce, &name, None, None, None, None))?;

    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<Lease, String> {
        // A container of this name is a leftover of an earlier boot: one whose
        // process was killed between create and start, which `auto_remove`
        // never removes because it never ran, or one still being removed.
        remove_by_name(&name)?;
        let container_id = run(dir, spec, scope, &name, &nonce)?;
        let port = mapped_port(&container_id, primary_container_port(spec)?)?;

        write_state(
            dir,
            &state("booting", &nonce, &name, Some(&container_id), Some(port), None, None),
        )?;

        refuse_a_blind_container(&container_id)?;
        wait_ready(&container_id, spec)?;

        let context = Boot {
            container_id: container_id.clone(),
            name: name.clone(),
            port,
            nonce: nonce.clone(),
        };
        boot(&context)?;

        write_state(
            dir,
            &state("ready", &nonce, &name, Some(&container_id), Some(port), None, None),
        )?;

        Ok(Lease {
            container_id,
            port,
            nonce: nonce.clone(),
            booted: true,
            dir: dir.to_path_buf(),
            _session: Arc::clone(session),
        })
    }));

    match outcome {
        Ok(Ok(lease)) => Ok(lease),
        Ok(Err(error)) => Err(fail_boot(dir, &name, &nonce, error)),
        Err(payload) => Err(fail_boot(dir, &name, &nonce, panic_text(payload))),
    }
}

/// Record a failed boot and remove the container it may have started, so no
/// waiter retries the boot and no orphaned container outlives it.
fn fail_boot(dir: &Path, name: &str, nonce: &str, error: String) -> String {
    let error = match remove_by_name(name) {
        Ok(()) => error,
        Err(leftover) => format!("{error}; {leftover}"),
    };
    let _ = write_state(
        dir,
        &state("failed", nonce, name, None, None, Some(&error), Some(now_millis())),
    );
    error
}

/// One state document, with the fields a status does not carry left null.
fn state(
    status: &str,
    nonce: &str,
    name: &str,
    container_id: Option<&str>,
    port: Option<u16>,
    error: Option<&str>,
    failed_at: Option<u64>,
) -> serde_json::Value {
    serde_json::json!({
        "nonce": nonce,
        "status": status,
        "name": name,
        "container_id": match container_id {
            Some(id) => serde_json::Value::from(id),
            None => serde_json::Value::Null,
        },
        "port": match port {
            Some(port) => serde_json::Value::from(port),
            None => serde_json::Value::Null,
        },
        "error": match error {
            Some(error) => serde_json::Value::from(error),
            None => serde_json::Value::Null,
        },
        "failed_at": match failed_at {
            Some(failed_at) => serde_json::Value::from(failed_at),
            None => serde_json::Value::Null,
        },
    })
}

fn panic_text(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "the shared server boot panicked".to_owned()
    }
}

/// The container port of the server's primary published port.
fn primary_container_port(spec: &Spec) -> Result<u16, String> {
    spec.ports
        .first()
        .map(|port| port.container)
        .ok_or_else(|| "the server recipe publishes no port".to_owned())
}

/// Reserve a free host port when the recipe asks for one.
///
/// # Errors
/// When the recipe reserves more than one host port, because
/// [`HOST_PORT_TOKEN`] names a single port, or no free port can be bound.
fn reserve_host_port(spec: &Spec) -> Result<Option<u16>, String> {
    let mut reserved = None;
    for port in &spec.ports {
        if matches!(port.host, HostPort::Reserved) {
            if reserved.is_some() {
                return Err(
                    "the server recipe reserves more than one host port, but the arguments \
                     name only one"
                        .to_owned(),
                );
            }
            reserved = Some(free_host_port()?);
        }
    }
    Ok(reserved)
}

/// A free host port, bound and released so the daemon can bind it.
fn free_host_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("could not reserve a host port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("could not read the reserved host port: {error}"))
}

/// Replace [`HOST_PORT_TOKEN`] in `args` with the reserved host port.
///
/// # Errors
/// When an argument names the token but the recipe reserves no port.
fn substitute_host_port(args: &[String], reserved: Option<u16>) -> Result<Vec<String>, String> {
    let mut substituted = Vec::with_capacity(args.len());
    for arg in args {
        if arg.contains(HOST_PORT_TOKEN) {
            let port = reserved.ok_or_else(|| {
                format!("the argument {arg:?} names {HOST_PORT_TOKEN}, but no port reserves one")
            })?;
            substituted.push(arg.replace(HOST_PORT_TOKEN, &port.to_string()));
        } else {
            substituted.push(arg.clone());
        }
    }
    Ok(substituted)
}

/// Create and start the container `spec` describes through the daemon API.
///
/// The daemon API has no single create-and-start call, so a process killed
/// between the two leaves a container in the `created` state: `auto_remove`
/// only acts on a container that ran and exited, and the watchdog never
/// started, so nothing in the container removes it. It carries the scope's
/// deterministic name, and the next boot of the same scope removes it by that
/// name before creating its own; until a later boot of that scope, it stays.
fn run(dir: &Path, spec: &Spec, scope: &Scope, name: &str, nonce: &str) -> Result<String, String> {
    let uid = uid();
    let reserved = reserve_host_port(spec)?;
    let args = substitute_host_port(&spec.args, reserved)?;

    let mut labels: HashMap<String, String> = HashMap::new();
    labels.insert(SESSION_LABEL.to_owned(), nonce.to_owned());
    labels.insert(NONCE_LABEL.to_owned(), nonce.to_owned());
    labels.insert(DIR_LABEL.to_owned(), dir.display().to_string());
    labels.insert(UID_LABEL.to_owned(), uid.to_string());
    labels.insert(INPUTS_LABEL.to_owned(), spec.inputs.clone());

    let mut exposed_ports = Vec::new();
    let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    for port in &spec.ports {
        exposed_ports.push(format!("{}/tcp", port.container));
        let host_port = match port.host {
            HostPort::Assigned => None,
            HostPort::Reserved => reserved.map(|port| port.to_string()),
        };
        port_bindings.insert(
            format!("{}/tcp", port.container),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_owned()),
                host_port,
            }]),
        );
    }

    let mut command = Vec::new();
    if matches!(spec.entrypoint, Entrypoint::Image) {
        command.push(spec.watchdog.clone());
    }
    command.push(scope.idle_grace.as_secs().to_string());
    command.push(nonce.to_owned());
    command.extend(args);

    let environment: Vec<String> = spec
        .environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();

    let host_config = HostConfig {
        auto_remove: Some(true),
        binds: Some(vec![format!(
            "{}:{CONTAINER_DIR}",
            dir.display()
        )]),
        port_bindings: Some(port_bindings),
        ..HostConfig::default()
    };

    let config = ContainerCreateBody {
        image: Some(spec.image.clone()),
        hostname: Some(name.to_owned()),
        labels: Some(labels),
        env: Some(environment),
        cmd: Some(command),
        entrypoint: match spec.entrypoint {
            Entrypoint::Watchdog => Some(vec![spec.watchdog.clone()]),
            Entrypoint::Image => None,
        },
        exposed_ports: Some(exposed_ports),
        host_config: Some(host_config),
        ..ContainerCreateBody::default()
    };

    let options = CreateContainerOptionsBuilder::new().name(name).build();
    let docker = docker();
    let created = block_on(docker.create_container(Some(options), config))
        .map_err(|error| format!("the Docker daemon did not create the container: {error}"))?;
    block_on(docker.start_container(&created.id, None))
        .map_err(|error| format!("the Docker daemon did not start the container: {error}"))?;
    Ok(created.id)
}

/// Refuse a container whose flock does not contend with the host's lease.
///
/// The host holds a shared lock on the bind-mounted lease. If the container can
/// take it exclusively it is not looking at the host's file - a remote daemon, a
/// mount that is not shared - and the watchdog could never see the lease, so the
/// container is removed and the boot refused loudly.
///
/// # Errors
/// When the container has no `flock`, cannot see the lease, or can take it while
/// the host holds it.
pub fn refuse_a_blind_container(container_id: &str) -> Result<(), String> {
    let list = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect::<Vec<_>>();
    let has_flock = exec_with_timeout(
        container_id,
        list(&["sh", "-c", "command -v flock"]),
        EXEC_TIMEOUT,
    )?;
    if !has_flock.success() {
        remove_by_id(container_id);
        return Err(format!(
            "the shared server image at {container_id} has no flock, so its watchdog \
             cannot watch the lease (every exec also runs under the image's `timeout`): {}",
            has_flock.stderr_text().trim()
        ));
    }
    // A bind mount the container cannot see makes `flock` fail with "no such
    // file", which is indistinguishable from a lock it correctly lost, so the
    // file has to be present before the contention means anything.
    let visible = exec_with_timeout(
        container_id,
        list(&["test", "-f", CONTAINER_LEASE]),
        EXEC_TIMEOUT,
    )?;
    if !visible.success() {
        remove_by_id(container_id);
        return Err(format!(
            "the container {container_id} cannot see {CONTAINER_LEASE}, so its watchdog \
             could never watch the lease; refusing the boot"
        ));
    }
    // `-E 75` makes a contended lock exit 75 rather than 1, so a probe that
    // fails for any other reason is told apart from one that correctly contends.
    let probe = exec_with_timeout(
        container_id,
        list(&[
            "flock",
            "-n",
            "-E",
            "75",
            "-x",
            CONTAINER_LEASE,
            "true",
        ]),
        EXEC_TIMEOUT,
    )?;
    if probe.success() {
        remove_by_id(container_id);
        return Err(format!(
            "the container {container_id} took the lease while this process holds it, \
             so it cannot see the host's lease; refusing the boot"
        ));
    }
    if probe.exit_code != Some(75) {
        remove_by_id(container_id);
        return Err(format!(
            "the container {container_id}'s lease probe did not contend with the host's \
             lease (exit {:?}): {}",
            probe.exit_code,
            probe.stderr_text().trim()
        ));
    }
    Ok(())
}

/// Wait until the container's server has finished initialising and accepts
/// connections.
///
/// A container that exits while it boots fails the wait at once, carrying the
/// last logs read from it, rather than leaving the boot to run out its whole
/// [`BOOT_TIMEOUT`] against a server that is gone. A daemon that cannot hand
/// over the logs of a container it still runs is an error, not an empty log.
fn wait_ready(container_id: &str, spec: &Spec) -> Result<(), String> {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut initialised = false;
    let mut last_logs = String::new();
    loop {
        let exited = |last_logs: &str| {
            let tail: Vec<&str> = last_logs.lines().rev().take(20).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            format!(
                "the shared server {container_id} exited while it booted; its last logs:\n{}",
                tail.join("\n")
            )
        };
        if !matches!(presence(container_id)?, Presence::Running { .. }) {
            return Err(exited(&last_logs));
        }
        if !initialised {
            match logs(container_id) {
                Ok(text) => {
                    initialised = text.contains(&spec.ready.log_marker);
                    last_logs = text;
                }
                Err(error) => {
                    if matches!(presence(container_id)?, Presence::Running { .. }) {
                        return Err(error);
                    }
                    return Err(exited(&last_logs));
                }
            }
        }
        if initialised
            && exec_with_timeout(container_id, spec.ready.probe.clone(), ANSWER_PROBE_TIMEOUT)
                .is_ok_and(|output| output.success())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the shared server {container_id} was not ready within {BOOT_TIMEOUT:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The host port the daemon mapped `container_id`'s `port` to.
///
/// Public so a multi-port server - one container running several node
/// processes, each on its own port, as the shared Dragonfly cluster does - can
/// resolve the mapped ports a [`Spec`] did not name as primary. A port mapping
/// is fixed at container creation, so any process holding the lease may call
/// this at any time, not only from inside the boot closure.
///
/// # Errors
/// When the daemon cannot be asked, or it names no mapped host port for `port`.
pub fn mapped_port(container_id: &str, port: u16) -> Result<u16, String> {
    let docker = docker();
    let inspect = block_on(docker.inspect_container(container_id, None))
        .map_err(|error| format!("inspect {container_id} for its mapped port failed: {error}"))?;
    let key = format!("{port}/tcp");
    inspect
        .network_settings
        .as_ref()
        .and_then(|settings| settings.ports.as_ref())
        .and_then(|ports| ports.get(&key))
        .and_then(|bindings| bindings.as_ref())
        .and_then(|bindings| bindings.first())
        .and_then(|binding| binding.host_port.as_ref())
        .and_then(|host_port| host_port.parse::<u16>().ok())
        .ok_or_else(|| format!("the daemon mapped no host port for {container_id}'s {key}"))
}

/// Remove a container by name, then wait for the daemon to report it gone.
///
/// Removal is asynchronous: a forced remove or the `auto_remove` policy returns
/// before the container disappears, and a boot that creates the same name
/// immediately after races the daemon and gets a name conflict. Waiting on the
/// observable `presence` closes that window.
///
/// # Errors
/// When the daemon cannot be asked, or the container is still present
/// [`REMOVE_WAIT`] after its removal was requested; the error names it, so the
/// boot reports the leftover instead of failing later on a name conflict.
fn remove_by_name(name: &str) -> Result<(), String> {
    // The remove's own answer is not the verdict: a container already being
    // removed refuses a second removal, and one that is gone refuses it too.
    // What the daemon reports afterwards is.
    let _ = remove(name);
    wait_gone(name, REMOVE_WAIT, presence)
}

/// Wait up to `wait` for `presence` to report `name` gone.
fn wait_gone(
    name: &str,
    wait: Duration,
    mut presence: impl FnMut(&str) -> Result<Presence, String>,
) -> Result<(), String> {
    let deadline = Instant::now() + wait;
    loop {
        let seen = presence(name)?;
        if matches!(seen, Presence::Gone) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the container {name} was still present ({seen:?}) {wait:?} after its removal \
                 was requested; remove it by that name and run again"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Remove a container by id if it exists.
pub fn remove_by_id(container_id: &str) {
    let _ = remove(container_id);
}

fn remove(container_id: &str) -> Result<(), String> {
    let options = RemoveContainerOptionsBuilder::new().force(true).v(true).build();
    let docker = docker();
    block_on(docker.remove_container(container_id, Some(options)))
        .map_err(|error| format!("remove {container_id} failed: {error}"))
}

/// Stop `container_id`, leaving it for its `auto_remove` policy to delete.
///
/// # Errors
/// When the daemon cannot stop it.
pub fn stop_container(container_id: &str) -> Result<(), String> {
    let docker = docker();
    block_on(docker.stop_container(container_id, None))
        .map_err(|error| format!("stop {container_id} failed: {error}"))
}

/// Run `command` in `container_id` and return its output, bounded by
/// [`EXEC_TIMEOUT`].
///
/// # Errors
/// When the daemon cannot create or run the exec, or it runs past the timeout.
pub fn exec_in_container(container_id: &str, command: &[&str]) -> Result<ExecOutput, String> {
    exec_bounded(container_id, command, EXEC_TIMEOUT)
}

/// Run `command` in `container_id` and return its output, bounded by
/// `timeout`; see [`exec_with_timeout`] for what the bound covers.
///
/// # Errors
/// When the daemon cannot create or run the exec, or it runs past `timeout`.
pub fn exec_bounded(
    container_id: &str,
    command: &[&str],
    timeout: Duration,
) -> Result<ExecOutput, String> {
    exec_with_timeout(
        container_id,
        command.iter().map(|argument| (*argument).to_owned()).collect(),
        timeout,
    )
}

/// The daemon's status for `container_id`, or `None` once it is gone.
///
/// # Panics
/// When the daemon answers with anything but the container's state or its own
/// "No such container": a failure to ask is not an absence, and a leak check
/// that read it as one would pass over a daemon it could not reach.
#[must_use]
pub fn container_status(container_id: &str) -> Option<String> {
    let docker = docker();
    match block_on(docker.inspect_container(container_id, None)) {
        Ok(inspect) => Some(
            inspect
                .state
                .and_then(|state| state.status)
                .map_or_else(|| "unknown".to_owned(), |status| status.to_string()),
        ),
        Err(error) if is_absent(&error) => None,
        Err(error) => panic!("the Docker daemon could not say whether {container_id} exists: {error}"),
    }
}

/// The deterministic name the boot of the scope at `dir` gives its container.
///
/// `dir` is the scope's canonical directory, the path its containers carry in
/// [`DIR_LABEL`].
#[must_use]
pub fn container_name(dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dir.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("zeroship-testkit-{}-{}", uid(), &digest[..12])
}

/// Create a container named `name` from `image` without starting it: the state a
/// boot killed between create and start leaves behind.
///
/// # Errors
/// When the daemon cannot create it.
pub fn create_unstarted(image: &str, name: &str) -> Result<String, String> {
    let config = ContainerCreateBody {
        image: Some(image.to_owned()),
        cmd: Some(vec!["sleep".to_owned(), "600".to_owned()]),
        host_config: Some(HostConfig {
            auto_remove: Some(true),
            ..HostConfig::default()
        }),
        ..ContainerCreateBody::default()
    };
    let options = CreateContainerOptionsBuilder::new().name(name).build();
    block_on(docker().create_container(Some(options), config))
        .map(|created| created.id)
        .map_err(|error| format!("the daemon did not create {name}: {error}"))
}

/// A running container, as the daemon lists it.
#[derive(Debug, Clone)]
pub struct Listed {
    /// The container's full id.
    pub id: String,
    /// Every label the container was created with.
    pub labels: HashMap<String, String>,
}

/// The running container that publishes `host_port` on the host, if any.
///
/// This is how a test asks who serves an address it was handed: the daemon's
/// own port table names the container behind a host port, whichever process
/// started it.
///
/// # Errors
/// When the daemon cannot list containers, or more than one running container
/// claims the port.
pub fn container_publishing(host_port: u16) -> Result<Option<Listed>, String> {
    use testcontainers::bollard::query_parameters::ListContainersOptionsBuilder;
    let options = ListContainersOptionsBuilder::new().all(false).build();
    let docker = docker();
    let containers = block_on(docker.list_containers(Some(options)))
        .map_err(|error| format!("list running containers failed: {error}"))?;
    let mut publishing: Vec<Listed> = containers
        .into_iter()
        .filter(|container| {
            container.ports.as_ref().is_some_and(|ports| {
                ports
                    .iter()
                    .any(|port| port.public_port == Some(host_port))
            })
        })
        .filter_map(|container| {
            Some(Listed {
                id: container.id?,
                labels: container.labels.unwrap_or_default(),
            })
        })
        .collect();
    match publishing.len() {
        0 => Ok(None),
        1 => Ok(publishing.pop()),
        _ => Err(format!(
            "{} running containers publish host port {host_port}: {:?}",
            publishing.len(),
            publishing.iter().map(|listed| &listed.id).collect::<Vec<_>>()
        )),
    }
}

/// The full ids of the containers carrying `DIR_LABEL=dir`, running or not.
///
/// # Errors
/// When the daemon cannot list containers.
pub fn containers_in_dir(dir: &str) -> Result<Vec<String>, String> {
    use testcontainers::bollard::query_parameters::ListContainersOptionsBuilder;
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    filters.insert(
        "label".to_owned(),
        vec![format!("{DIR_LABEL}={dir}")],
    );
    let options = ListContainersOptionsBuilder::new()
        .all(true)
        .filters(&filters)
        .build();
    let docker = docker();
    let containers = block_on(docker.list_containers(Some(options)))
        .map_err(|error| format!("list containers for {dir} failed: {error}"))?;
    Ok(containers
        .into_iter()
        .filter_map(|container| container.id)
        .collect())
}

/// Create and start a detached, `auto_remove` container that sleeps, for a
/// contract test that drives the lease refusal or a lifetime measurement
/// directly.
///
/// # Errors
/// When the daemon cannot create or start it.
pub fn run_detached(
    image: &str,
    binds: &[String],
    labels: &[(&str, &str)],
    name: &str,
) -> Result<String, String> {
    let host_config = HostConfig {
        auto_remove: Some(true),
        binds: Some(binds.to_vec()),
        ..HostConfig::default()
    };
    let config = ContainerCreateBody {
        image: Some(image.to_owned()),
        hostname: Some(name.to_owned()),
        labels: Some(
            labels
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        ),
        cmd: Some(vec!["sleep".to_owned(), "600".to_owned()]),
        host_config: Some(host_config),
        ..ContainerCreateBody::default()
    };
    let options = CreateContainerOptionsBuilder::new().name(name).build();
    let docker = docker();
    let created = block_on(docker.create_container(Some(options), config))
        .map_err(|error| format!("the daemon did not create {name}: {error}"))?;
    block_on(docker.start_container(&created.id, None))
        .map_err(|error| format!("the daemon did not start {name}: {error}"))?;
    Ok(created.id)
}

/// Run one statement as the superuser through a container's `psql`.
///
/// # Errors
/// When the container cannot run `psql`, the statement fails, or the statement
/// runs past [`EXEC_TIMEOUT`].
pub fn psql(container_id: &str, database: &str, sql: &str) -> Result<(), String> {
    let output = exec_with_timeout(
        container_id,
        vec![
            "psql".to_owned(),
            "-U".to_owned(),
            "postgres".to_owned(),
            "-d".to_owned(),
            database.to_owned(),
            "-v".to_owned(),
            "ON_ERROR_STOP=1".to_owned(),
            "-c".to_owned(),
            sql.to_owned(),
        ],
        EXEC_TIMEOUT,
    )?;
    if output.success() {
        return Ok(());
    }
    Err(format!(
        "psql on {container_id} failed (exit {:?}): {}",
        output.exit_code,
        output.stderr_text().trim()
    ))
}

/// The daemon's logs for `container_id`, standard output and error together.
fn logs(container_id: &str) -> Result<String, String> {
    let options = LogsOptionsBuilder::new()
        .stdout(true)
        .stderr(true)
        .build();
    let docker = docker();
    let stream = docker.logs(container_id, Some(options));
    block_on(async {
        futures::pin_mut!(stream);
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(output) => text.push_str(&String::from_utf8_lossy(output.as_ref())),
                Err(error) => return Err(format!("reading {container_id}'s logs failed: {error}")),
            }
        }
        Ok(text)
    })
}

/// Run `command` in `container_id`, stopping it once `timeout` passes.
///
/// The bound covers the whole exchange - creating the exec, attaching, reading
/// its output to the end and reading its exit code - not just the attach, so
/// a command that never ends cannot hold its caller. The command runs under
/// the container's own `timeout`, which sends it `SIGTERM` at the deadline and
/// `SIGKILL` a second later, so a stopped command does not linger in a server
/// other processes share; this side stops waiting [`EXEC_SLACK`] after that.
/// A command `timeout` stopped is reported as an error, never as its output.
fn exec_with_timeout(
    container_id: &str,
    command: Vec<String>,
    timeout: Duration,
) -> Result<ExecOutput, String> {
    let docker = docker();
    let mut bounded = vec![
        "timeout".to_owned(),
        "-k".to_owned(),
        "1".to_owned(),
        format!("{:.3}", timeout.as_secs_f64()),
    ];
    bounded.extend(command.iter().cloned());
    let exchange = async {
        let config = ExecConfig {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            cmd: Some(bounded),
            ..ExecConfig::default()
        };
        let created = docker
            .create_exec(container_id, config)
            .await
            .map_err(|error| format!("create exec on {container_id} failed: {error}"))?;
        let started = docker
            .start_exec(&created.id, None)
            .await
            .map_err(|error| format!("start exec on {container_id} failed: {error}"))?;
        let StartExecResults::Attached { output, .. } = started else {
            return Err(format!("exec on {container_id} did not attach"));
        };
        futures::pin_mut!(output);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(item) = output.next().await {
            match item {
                Ok(LogOutput::StdErr { message }) => stderr.extend_from_slice(&message),
                Ok(other) => stdout.extend_from_slice(other.as_ref()),
                Err(error) => {
                    return Err(format!("reading exec output on {container_id} failed: {error}"))
                }
            }
        }
        let inspect: ExecInspectResponse = docker
            .inspect_exec(&created.id)
            .await
            .map_err(|error| format!("inspect exec on {container_id} failed: {error}"))?;
        Ok(ExecOutput {
            stdout,
            stderr,
            exit_code: inspect.exit_code,
        })
    };
    // The timer is created inside the runtime: a tokio timer built on the
    // caller's thread finds no time driver there.
    let output = block_on(async { tokio::time::timeout(timeout + EXEC_SLACK, exchange).await })
        .map_err(|_| {
            format!(
                "exec {command:?} on {container_id} timed out: the daemon did not finish the \
                 exchange within {:?}",
                timeout + EXEC_SLACK
            )
        })??;
    if output.exit_code == Some(TIMED_OUT) {
        return Err(format!(
            "exec {command:?} on {container_id} timed out: it ran past {timeout:?} and the \
             container's `timeout` stopped it"
        ));
    }
    Ok(output)
}

/// The inputs a server's directory is named after.
#[must_use]
pub fn digest(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let digest = format!("{:x}", hasher.finalize());
    digest[..12].to_string()
}

/// A value no other boot shares, safe across processes and pids.
fn mint_nonce() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        now_millis(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The uid of the user running the tests.
#[must_use]
pub fn uid() -> u32 {
    fs::metadata("/proc/self").map_or(0, |metadata| metadata.uid())
}

/// The repository root, two directories above this crate.
pub(crate) fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the shared-server crate lives under crates/")
        .to_owned()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn open_lock(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))
}

/// Take a shared lock on `path`, waiting up to [`LEASE_WAIT`].
fn lock_shared(path: &Path) -> Result<File, String> {
    let file = open_lock(path)?;
    let deadline = Instant::now() + LEASE_WAIT;
    loop {
        match flock(&file, libc::LOCK_SH | libc::LOCK_NB) {
            Ok(()) => return Ok(file),
            Err(error) if error.raw_os_error() == Some(libc::EWOULDBLOCK) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "waited {LEASE_WAIT:?} for the shared lease on {}; another process is \
                         booting the server or its watchdog is tearing the old one down",
                        path.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format!("shared lock on {}: {error}", path.display())),
        }
    }
}

/// Take an exclusive lock on `file`, waiting up to `wait`.
fn lock_exclusive(file: &File, wait: Duration) -> Result<(), String> {
    let deadline = Instant::now() + wait;
    loop {
        match flock(file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => return Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::EWOULDBLOCK) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "waited {wait:?} for the boot lock; another process is still booting \
                         the shared server"
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format!("boot lock: {error}")),
        }
    }
}

#[expect(
    unsafe_code,
    reason = "flock is a libc call with no safe wrapper among this crate's dependencies"
)]
fn flock(file: &File, operation: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `file` owns a valid descriptor for the duration of the call, and
    // flock only reads that descriptor.
    let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn read_state(dir: &Path) -> Option<serde_json::Value> {
    let text = fs::read_to_string(dir.join(STATE_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_state(dir: &Path, state: &serde_json::Value) -> Result<(), String> {
    let path = dir.join(STATE_FILE);
    let temporary = dir.join(format!("{STATE_FILE}.tmp"));
    let text = serde_json::to_string(state).map_err(|error| error.to_string())?;
    fs::write(&temporary, text)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .map_err(|error| format!("could not rename {}: {error}", temporary.display()))
}

/// Whether the daemon already carries an image by `reference`.
#[must_use]
pub fn image_exists(reference: &str) -> bool {
    let docker = docker();
    block_on(docker.inspect_image(reference)).is_ok()
}

/// The content-addressed id (`sha256:...`) of the image the daemon holds under
/// `reference`.
///
/// A tag names a recipe; the id names one build of it. Two builds of a recipe
/// that generates material at build time - keys, say - carry one tag and two
/// ids, so a derived image keyed by the id cannot outlive the build it copied.
///
/// # Errors
/// When the daemon does not hold `reference` or cannot be asked.
pub fn image_id(reference: &str) -> Result<String, String> {
    let docker = docker();
    block_on(docker.inspect_image(reference))
        .map_err(|error| format!("inspect image {reference} failed: {error}"))?
        .id
        .ok_or_else(|| format!("the daemon reported no id for image {reference}"))
}

/// The multi-threaded runtime every bollard call blocks on.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .thread_name("zeroship-testkit")
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime for the shared-server daemon calls")
    })
}

/// Block on `future` on the testkit's runtime.
fn block_on<F: Future>(future: F) -> F::Output {
    runtime().block_on(future)
}

/// The configured Docker client, created once per process.
fn docker() -> Docker {
    static DOCKER: OnceLock<Docker> = OnceLock::new();
    DOCKER
        .get_or_init(|| {
            block_on(testcontainers::core::client::docker_client_instance())
                .expect("a Docker client for the shared testkit server")
        })
        .clone()
}
#[cfg(test)]
mod tests {
    use super::*;
    use testcontainers::bollard::errors::Error;

    fn server_error(status_code: u16, message: &str) -> Error {
        Error::DockerResponseServerError {
            status_code,
            message: message.to_owned(),
        }
    }

    #[test]
    fn only_the_daemons_no_such_container_answer_is_absence() {
        assert!(is_absent(&server_error(404, "No such container: abc")));
        assert!(!is_absent(&server_error(
            500,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock"
        )));
        assert!(!is_absent(&server_error(404, "page not found")));
        assert!(!is_absent(&server_error(409, "removal of container abc is already in progress")));
        assert!(!is_absent(&Error::RequestTimeoutError));
    }

    #[test]
    fn a_name_that_disappears_is_waited_for() {
        let mut answers = vec![
            Presence::Running {
                session: "a".to_owned(),
            },
            Presence::Stopped,
            Presence::Gone,
        ]
        .into_iter();
        wait_gone("leftover", Duration::from_secs(5), |_| {
            Ok(answers.next().expect("asked past the last answer"))
        })
        .expect("a container that disappears is waited out");
    }

    #[test]
    fn a_name_that_never_disappears_is_reported_by_name() {
        let error = wait_gone("zeroship-testkit-leftover", Duration::from_millis(200), |_| {
            Ok(Presence::Stopped)
        })
        .expect_err("a container that stays must be reported");
        assert!(error.contains("zeroship-testkit-leftover"), "{error}");
        assert!(error.contains("still present"), "{error}");
    }

    #[test]
    fn a_daemon_that_cannot_be_asked_fails_the_wait() {
        let error = wait_gone("leftover", Duration::from_secs(5), |_| {
            Err("inspect leftover failed: the daemon is unreachable".to_owned())
        })
        .expect_err("a daemon error is not an absence");
        assert!(error.contains("unreachable"), "{error}");
    }
}
