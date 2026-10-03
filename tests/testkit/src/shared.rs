//! One server shared by every test process of a worktree.
//!
//! Every test binary of a run - nextest's process-per-test, several `cargo test`
//! binaries, an xtask-held run - joins the same server through this module. The
//! processes elect one booter with an exclusive `flock` on a per-worktree
//! directory under `target/`, then every process holds a shared lock on the same
//! directory as its lease. A container-side watchdog watches that lease and
//! removes the container once no process has held it for the idle grace. The
//! server is whatever [`Spec`] describes: the platform database, a bare
//! PostgreSQL server, or a Redpanda broker.
//!
//! There is no environment variable anywhere in the protocol: a process learns
//! the directory from the repository it was compiled in and the scope it is
//! given, and the container learns its lease from the bind-mounted directory.
//! Plain `cargo test` and nextest therefore take the same path.
//!
//! The directory holds three files:
//! - `session.lock`, whose shared lock is a process's lease and whose exclusive
//!   lock is what the watchdog probes to decide the server is idle;
//! - `boot.lock`, held exclusively by whichever process is booting, so the
//!   others wait rather than race;
//! - `state.json`, written by atomic rename, naming the container, its port and
//!   whether the boot is `booting`, `ready` or `failed`.

use std::any::Any;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::docker::{DockerCli, OWNER_LABEL};

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

/// How long a boot gets from `docker run` to a migrated database.
const BOOT_TIMEOUT: Duration = Duration::from_mins(5);

/// How long one `docker exec` statement may run before it is killed.
const EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a `docker exec psql` probe may run before it counts as unanswered.
const ANSWER_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a server that is present may go without answering before this
/// caller reports it unreachable.
const ANSWER_DEADLINE: Duration = Duration::from_secs(15);

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
        Self {
            kind: kind.to_owned(),
            explicit: None,
            idle_grace: DEFAULT_IDLE_GRACE,
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

/// A boot's view of the container it started, handed to the boot closure.
#[derive(Debug)]
pub struct Boot {
    /// The `docker` program the container was started through.
    pub docker: DockerCli,
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
        psql(&self.docker, &self.container_id, database, sql)
    }

    /// Run a program in the container and return its output.
    ///
    /// # Errors
    /// When the program cannot be run.
    pub fn exec(&self, program: &str, args: &[&str]) -> Result<Output, String> {
        self.docker
            .command()
            .args(["exec", &self.container_id, program])
            .args(args)
            .output()
            .map_err(|error| format!("{} exec {program}: {error}", self.docker.describe()))
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

/// Docker's view of a container named by a ready state.
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
    let docker = DockerCli::system();
    let dir = scope.directory(&spec.inputs)?;
    let session = Arc::new(lock_shared(&dir.join("session.lock"))?);

    match ready(&docker, &dir, spec, &session)? {
        Ready::Joined(lease) => return Ok(lease),
        Ready::Unreachable(error) => return Err(error),
        Ready::Absent => {}
    }

    let boot_lock = open_lock(&dir.join("boot.lock"))?;
    lock_exclusive(&boot_lock, BOOT_LOCK_WAIT)?;

    match ready(&docker, &dir, spec, &session)? {
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

    boot_server(&docker, &dir, spec, scope, &session, boot)
}

/// Resolve a ready state to a server this process can lease.
///
/// A live container that does not answer is reported, never replaced: replacing
/// it would destroy a healthy server for every other process because one caller
/// could not reach it. Only a container the daemon reports gone or stopped, or
/// whose session label names a different boot, is treated as absent.
fn ready(docker: &DockerCli, dir: &Path, spec: &Spec, session: &Arc<File>) -> Result<Ready, String> {
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

    match presence(docker, container_id)? {
        Presence::Gone | Presence::Stopped => return Ok(Ready::Absent),
        Presence::Running { session: live } => {
            if live != nonce {
                return Ok(Ready::Absent);
            }
        }
    }

    if answers(docker, container_id, &spec.ready.answer) {
        return Ok(Ready::Joined(Lease {
            container_id: container_id.to_owned(),
            port: port as u16,
            nonce: nonce.to_owned(),
            booted: false,
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
/// server costs one `docker exec` rather than the whole probe budget.
fn answers(docker: &DockerCli, container_id: &str, answer: &[String]) -> bool {
    let deadline = Instant::now() + ANSWER_DEADLINE;
    loop {
        let mut command = docker.command();
        command.args(["exec", container_id]).args(answer);
        if let Ok(output) = output_with_timeout(command, ANSWER_PROBE_TIMEOUT) {
            if output.status.success() {
                return true;
            }
            let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
            if stderr.contains("no such container") {
                return false;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What the daemon says about a container named by a ready state.
fn presence(docker: &DockerCli, container_id: &str) -> Result<Presence, String> {
    let output = docker
        .command()
        .args([
            "inspect",
            "--format",
            "{{.State.Status}}\t{{index .Config.Labels \"zeroship.testkit.session\"}}",
        ])
        .arg(container_id)
        .output()
        .map_err(|error| format!("{} inspect: {error}", docker.describe()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if stderr.contains("no such container") || stderr.contains("no such object") {
            return Ok(Presence::Gone);
        }
        return Err(format!(
            "{} inspect {container_id} failed ({}): {}",
            docker.describe(),
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut fields = stdout.trim().split('\t');
    let status = fields.next().unwrap_or_default();
    let session = fields.next().unwrap_or_default().to_owned();
    if status == "running" {
        Ok(Presence::Running { session })
    } else {
        Ok(Presence::Stopped)
    }
}

/// Boot a server in `dir` while holding the boot lock and the session lease.
fn boot_server(
    docker: &DockerCli,
    dir: &Path,
    spec: &Spec,
    scope: &Scope,
    session: &Arc<File>,
    boot: impl FnOnce(&Boot) -> Result<(), String>,
) -> Result<Lease, String> {
    let nonce = mint_nonce();
    let name = container_name(dir);
    let mut reaper = Reaper::spawn(docker, &nonce)?;

    write_state(dir, &state("booting", &nonce, &name, None, None, None, None))?;

    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<Lease, String> {
        remove_by_name(docker, &name);
        let container_id = run(docker, dir, spec, scope, &name, &nonce)?;
        let port = mapped_port(docker, &container_id, primary_container_port(spec)?)?;

        write_state(
            dir,
            &state("booting", &nonce, &name, Some(&container_id), Some(port), None, None),
        )?;

        refuse_a_blind_container(docker, &container_id)?;
        reaper.disarm();
        wait_ready(docker, &container_id, spec)?;

        let context = Boot {
            docker: docker.clone(),
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
            _session: Arc::clone(session),
        })
    }));

    match outcome {
        Ok(Ok(lease)) => Ok(lease),
        Ok(Err(error)) => Err(fail_boot(docker, dir, &name, &nonce, error)),
        Err(payload) => Err(fail_boot(docker, dir, &name, &nonce, panic_text(payload))),
    }
}

/// Record a failed boot and remove the container it may have started, so no
/// waiter retries the boot and no orphaned container outlives it.
fn fail_boot(
    docker: &DockerCli,
    dir: &Path,
    name: &str,
    nonce: &str,
    error: String,
) -> String {
    remove_by_name(docker, name);
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

/// Start the container `spec` describes.
fn run(
    docker: &DockerCli,
    dir: &Path,
    spec: &Spec,
    scope: &Scope,
    name: &str,
    nonce: &str,
) -> Result<String, String> {
    let uid = uid();
    let reserved = reserve_host_port(spec)?;
    let args = substitute_host_port(&spec.args, reserved)?;
    let mut command = docker.command();
    command.args(["run", "--detach", "--rm", "--name", name, "--hostname", name]);
    for port in &spec.ports {
        let host = match port.host {
            HostPort::Assigned => None,
            HostPort::Reserved => reserved,
        };
        let publish = match host {
            Some(host) => format!("127.0.0.1:{host}:{}", port.container),
            None => format!("127.0.0.1::{}", port.container),
        };
        command.arg("--publish").arg(publish);
    }
    if matches!(spec.entrypoint, Entrypoint::Watchdog) {
        command.arg("--entrypoint").arg(&spec.watchdog);
    }
    command
        .arg("--mount")
        .arg(format!(
            "type=bind,src={},dst={CONTAINER_DIR}",
            dir.display()
        ))
        .args(["--label", &format!("{SESSION_LABEL}={nonce}")])
        .args(["--label", &format!("{NONCE_LABEL}={nonce}")])
        .args(["--label", &format!("{DIR_LABEL}={}", dir.display())])
        .args(["--label", &format!("{UID_LABEL}={uid}")])
        .args(["--label", &format!("{INPUTS_LABEL}={}", spec.inputs)])
        .args(["--label", &format!("{OWNER_LABEL}={}", crate::docker::process_owner())]);
    for (key, value) in &spec.environment {
        command.arg("--env").arg(format!("{key}={value}"));
    }
    command.arg(&spec.image);
    if matches!(spec.entrypoint, Entrypoint::Image) {
        command.arg(&spec.watchdog);
    }
    command
        .arg(scope.idle_grace.as_secs().to_string())
        .arg(nonce)
        .args(&args)
        .stdin(Stdio::null());

    let output = command
        .output()
        .map_err(|error| format!("{} run: {error}", docker.describe()))?;
    if !output.status.success() {
        return Err(format!(
            "{} run {name} failed ({}): {}",
            docker.describe(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() {
        return Err(format!("{} run {name} named no container", docker.describe()));
    }
    Ok(id)
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
pub fn refuse_a_blind_container(docker: &DockerCli, container_id: &str) -> Result<(), String> {
    let has_flock = output_with_timeout(
        {
            let mut command = docker.command();
            command.args(["exec", container_id, "sh", "-c", "command -v flock"]);
            command
        },
        EXEC_TIMEOUT,
    )
    .map_err(|error| format!("{} exec flock: {error}", docker.describe()))?;
    if !has_flock.status.success() {
        remove_by_id(docker, container_id);
        return Err(format!(
            "the shared server image at {container_id} has no flock, so its watchdog \
             cannot watch the lease"
        ));
    }
    // A bind mount the container cannot see makes `flock` fail with "no such
    // file", which is indistinguishable from a lock it correctly lost, so the
    // file has to be present before the contention means anything.
    let visible = output_with_timeout(
        {
            let mut command = docker.command();
            command.args(["exec", container_id, "test", "-f", CONTAINER_LEASE]);
            command
        },
        EXEC_TIMEOUT,
    )
    .map_err(|error| format!("{} exec test: {error}", docker.describe()))?;
    if !visible.status.success() {
        remove_by_id(docker, container_id);
        return Err(format!(
            "the container {container_id} cannot see {CONTAINER_LEASE}, so its watchdog \
             could never watch the lease; refusing the boot"
        ));
    }
    // `-E 75` makes a contended lock exit 75 rather than 1, so a probe that
    // fails for any other reason is told apart from one that correctly contends.
    let probe = output_with_timeout(
        {
            let mut command = docker.command();
            command.args(["exec", container_id, "flock", "-n", "-E", "75", "-x", CONTAINER_LEASE, "true"]);
            command
        },
        EXEC_TIMEOUT,
    )
    .map_err(|error| format!("{} exec flock: {error}", docker.describe()))?;
    if probe.status.success() {
        remove_by_id(docker, container_id);
        return Err(format!(
            "the container {container_id} took the lease while this process holds it, \
             so it cannot see the host's lease; refusing the boot"
        ));
    }
    if probe.status.code() != Some(75) {
        remove_by_id(docker, container_id);
        return Err(format!(
            "the container {container_id}'s lease probe did not contend with the host's \
             lease ({}): {}",
            probe.status,
            String::from_utf8_lossy(&probe.stderr).trim()
        ));
    }
    Ok(())
}

/// Wait until the container's server has finished initialising and accepts
/// connections.
fn wait_ready(docker: &DockerCli, container_id: &str, spec: &Spec) -> Result<(), String> {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut initialised = false;
    loop {
        if !initialised {
            let logs = docker
                .command()
                .args(["logs", container_id])
                .output()
                .map_err(|error| format!("{} logs: {error}", docker.describe()))?;
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&logs.stdout),
                String::from_utf8_lossy(&logs.stderr)
            );
            initialised = text.contains(&spec.ready.log_marker);
        }
        if initialised {
            let mut command = docker.command();
            command.args(["exec", container_id]).args(&spec.ready.probe);
            if let Ok(ready) = output_with_timeout(command, ANSWER_PROBE_TIMEOUT) {
                if ready.status.success() {
                    return Ok(());
                }
            }
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
fn mapped_port(docker: &DockerCli, container_id: &str, port: u16) -> Result<u16, String> {
    let output = docker
        .command()
        .args(["port", container_id, &format!("{port}/tcp")])
        .output()
        .map_err(|error| format!("{} port: {error}", docker.describe()))?;
    if !output.status.success() {
        return Err(format!(
            "{} port {container_id} failed ({}): {}",
            docker.describe(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.rsplit(':').next()?.trim().parse::<u16>().ok())
        .ok_or_else(|| {
            format!(
                "{} port {container_id} named no host port: {text:?}",
                docker.describe()
            )
        })
}

/// Remove a container by name if it exists.
fn remove_by_name(docker: &DockerCli, name: &str) {
    let _ = docker
        .command()
        .args(["rm", "--force", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Remove a container by id if it exists.
pub fn remove_by_id(docker: &DockerCli, container_id: &str) {
    let _ = docker
        .command()
        .args(["rm", "--force", container_id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Run one statement as the superuser through a container's `psql`.
///
/// # Errors
/// When the container cannot run `psql`, the statement fails, or the statement
/// runs past [`EXEC_TIMEOUT`].
pub fn psql(
    docker: &DockerCli,
    container_id: &str,
    database: &str,
    sql: &str,
) -> Result<(), String> {
    let output = output_with_timeout(
        {
            let mut command = docker.command();
            command
                .args(["exec", container_id, "psql", "-U", "postgres", "-d", database])
                .args(["-v", "ON_ERROR_STOP=1", "-c", sql]);
            command
        },
        EXEC_TIMEOUT,
    )
    .map_err(|error| format!("{} exec psql: {error}", docker.describe()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "psql on {container_id} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Run `command`, killing it once `timeout` passes.
fn output_with_timeout(mut command: Command, timeout: Duration) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run {:?}: {error}", command.get_program()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {timeout:?}"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format!("could not wait for the child: {error}")),
        }
    }
    child
        .wait_with_output()
        .map_err(|error| format!("could not read the child's output: {error}"))
}

/// The deterministic name of the container a scope's directory boots.
fn container_name(dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dir.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("zeroship-testkit-{}-{}", uid(), &digest[..12])
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

/// The repository root, two directories above the testkit crate.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("testkit lives under tests/")
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

/// A detached process that removes the boot's containers if its parent dies
/// before the boot is disarmed.
struct Reaper {
    process: Child,
    lifetime: ChildStdin,
}

impl Reaper {
    /// The reaper's script. `$1` is the `docker` program and `$2` the session
    /// label filter. A `disarm` line means the container is owned by its
    /// watchdog; end-of-file means the parent died and the containers must go.
    const SCRIPT: &'static str = r#"if read line && [ "$line" = disarm ]; then exit 0; fi
sweep() {
  ids=$("$1" ps --all --quiet --no-trunc --filter "$2")
  if [ -n "$ids" ]; then "$1" rm --force --volumes $ids; fi
}
sweep "$1" "$2"
sleep 5
sweep "$1" "$2""#;

    fn spawn(docker: &DockerCli, nonce: &str) -> Result<Self, String> {
        let mut process = Command::new("sh")
            .arg("-c")
            .arg(Self::SCRIPT)
            .arg("zeroship-testkit-reaper")
            .arg(docker.program())
            .arg(format!("label={SESSION_LABEL}={nonce}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|error| format!("could not start the boot reaper: {error}"))?;
        let lifetime = process
            .stdin
            .take()
            .expect("the reaper was spawned with a piped stdin");
        Ok(Self { process, lifetime })
    }

    fn disarm(&mut self) {
        let _ = self.lifetime.write_all(b"disarm\n");
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
