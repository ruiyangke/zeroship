//! Containers a test process owns, removed when that process ends.
//!
//! A fixture that keeps one server for a whole test binary keeps it in a `static`,
//! and libtest never drops a `static`: the container handle's `Drop`, which is what
//! removes a testcontainers container, never runs. Every run would leave its servers
//! behind.
//!
//! THE REAPER. [`start_owned`] first spawns a detached `sh` whose stdin is a pipe this
//! process holds open and never writes, and only then asks Docker for the container,
//! carrying a label minted for it ([`Ownership`]). The kernel closes the pipe when
//! this process ends - by returning, panicking, aborting or being killed, SIGKILL
//! included - and the reaper then removes every container carrying that label,
//! together with its anonymous volumes. Because the reaper exists before the container
//! does, a process killed while its container is still being created, started or
//! waited on for readiness leaves nothing behind either. It sweeps twice, a few
//! seconds apart, so a create the dying process still had in flight at the first
//! sweep is caught by the second. The pipe's write end is close-on-exec, so no other
//! child of this process inherits it and keeps the reaper waiting, and the reaper sits
//! in a process group of its own, so the terminal's Ctrl-C reaches the test process
//! and not the one that cleans up after it.
//!
//! THE CLI HAS TO SEE THE CONTAINER. testcontainers reaches Docker through its own
//! client configuration; the reaper reaches it through the `docker` CLI and that CLI's
//! configuration. The two can disagree - a docker context, or a testcontainers
//! properties file, that only one of them reads - and then the reaper's sweep finds
//! nothing and the leak is silent. So [`start_owned`] asks the CLI for the containers
//! carrying the new label, the reaper's own query through the reaper's own program,
//! and unless the answer names the container it removes it through the client that
//! created it and refuses the start.
//!
//! WHAT IT DOES NOT COVER. The reaper is a process too. Killing its process group, a
//! job sandbox that kills every descendant at once, or a Docker daemon that is
//! unreachable when the owner ends, each leaves the container behind.
//!
//! The `docker` program is a value rather than an environment lookup:
//! [`DockerCli::system`] runs `docker` from `PATH`, and a test hands [`DockerCli::at`]
//! a program of its own.
//!
//! [`lifetime`] holds the two measurements a fixture adopting this owes.

#![allow(
    dead_code,
    reason = "each crate that includes this fixture uses its own subset of it"
)]

#[path = "lifetime.rs"]
pub mod lifetime;

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use testcontainers::runners::SyncRunner;
use testcontainers::{Container, ContainerRequest, GenericImage, ImageExt};

/// The label every owned container carries, naming the process that owns it.
pub const OWNER_LABEL: &str = "zeroship.test.owner";

/// The label the reaper removes by, unique to one container.
pub const REAPER_LABEL: &str = "zeroship.test.reaper";

/// This process's [`OWNER_LABEL`] value: the pid, and the moment the value was first
/// asked for, so a pid the system hands out again later never names this process's
/// containers.
#[must_use]
pub fn process_owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        format!("{}-{nanos}", std::process::id())
    })
}

/// The `docker` program the reaper and the visibility check run.
#[derive(Debug, Clone)]
pub struct DockerCli {
    program: OsString,
}

impl DockerCli {
    /// `docker`, found on `PATH` when it runs.
    #[must_use]
    pub fn system() -> Self {
        Self::at("docker")
    }

    /// The program at `program`.
    #[must_use]
    pub fn at(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// The full ids of the containers carrying `key=value`, by the same query the
    /// reaper runs.
    ///
    /// # Errors
    /// When the program cannot be run or exits unsuccessfully, with what it said.
    pub fn labelled(&self, key: &str, value: &str) -> Result<Vec<String>, String> {
        let output = Command::new(&self.program)
            .args(["ps", "--all", "--quiet", "--no-trunc", "--filter"])
            .arg(format!("label={key}={value}"))
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("{} is not runnable: {error}", self.display()))?;
        if !output.status.success() {
            return Err(format!(
                "{} ps exited {}: {}",
                self.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn display(&self) -> String {
        format!("`{}`", self.program.to_string_lossy())
    }
}

/// The [`REAPER_LABEL`] value for one container, minted before the container exists.
#[derive(Debug)]
pub struct Ownership {
    reaper: String,
}

impl Ownership {
    /// A value no other container of any process carries.
    #[must_use]
    pub fn mint() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self {
            reaper: format!(
                "{}-{}",
                process_owner(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    /// The label value.
    #[must_use]
    pub fn reaper(&self) -> &str {
        &self.reaper
    }
}

/// A container this process started, with the reaper that removes it once the
/// process has ended.
///
/// Dropping it removes the container at once, through the container's own `Drop`, and
/// ends the reaper, whose sweep then finds nothing left to remove.
#[derive(Debug)]
pub struct OwnedContainer {
    container: Container<GenericImage>,
    _reaper: Reaper,
}

impl OwnedContainer {
    /// The container.
    #[must_use]
    pub const fn container(&self) -> &Container<GenericImage> {
        &self.container
    }
}

/// Start `request` as a container this process owns.
///
/// Spawns the reaper first, then starts the container carrying [`OWNER_LABEL`] and
/// [`REAPER_LABEL`], then requires `docker` to list it by the reaper's label. See the
/// module doc for why each step is in that order.
///
/// # Errors
/// When `docker` cannot be run, the reaper cannot be spawned, the daemon does not
/// start the container, or `docker` cannot see the container it started - in which
/// case the container has been removed again.
pub fn start_owned(
    docker: &DockerCli,
    ownership: &Ownership,
    request: ContainerRequest<GenericImage>,
) -> Result<OwnedContainer, String> {
    let reaper = Reaper::spawn(docker, ownership).map_err(|error| {
        format!("could not start the process that removes the container afterwards: {error}")
    })?;
    let container = request
        .with_label(OWNER_LABEL, process_owner())
        .with_label(REAPER_LABEL, ownership.reaper())
        .start()
        .map_err(|error| format!("the Docker daemon did not start the container: {error}"))?;
    let seen = docker.labelled(REAPER_LABEL, ownership.reaper());
    if matches!(&seen, Ok(ids) if ids.iter().any(|id| id == container.id())) {
        return Ok(OwnedContainer {
            container,
            _reaper: reaper,
        });
    }
    let id = container.id().to_string();
    let answer = match seen {
        Ok(ids) => format!("it listed {ids:?}"),
        Err(error) => error,
    };
    let removed = match container.rm() {
        Ok(()) => "it has been removed".to_string(),
        Err(error) => format!("removing it failed too: {error}"),
    };
    Err(format!(
        "{} removes container {id} once this process ends, but cannot see it ({answer}), \
         so the container would outlive the process; {removed}",
        docker.display()
    ))
}

/// A detached process that removes the containers carrying one [`REAPER_LABEL`]
/// value once this process has ended. See the module doc.
#[derive(Debug)]
struct Reaper {
    _process: Child,
    _lifetime: ChildStdin,
}

impl Reaper {
    /// The reaper's script. `$1` is the `docker` program and `$2` the label filter.
    const SCRIPT: &'static str = r#"cat >/dev/null
sweep() {
  ids=$("$1" ps --all --quiet --no-trunc --filter "$2")
  if [ -n "$ids" ]; then "$1" rm --force --volumes $ids; fi
}
sweep "$1" "$2"
sleep 5
sweep "$1" "$2""#;

    fn spawn(docker: &DockerCli, ownership: &Ownership) -> std::io::Result<Self> {
        let mut process = Command::new("sh")
            .arg("-c")
            .arg(Self::SCRIPT)
            .arg("zeroship-container-reaper")
            .arg(&docker.program)
            .arg(format!("label={REAPER_LABEL}={}", ownership.reaper()))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let lifetime = process
            .stdin
            .take()
            .expect("the reaper was spawned with a piped stdin");
        Ok(Self {
            _process: process,
            _lifetime: lifetime,
        })
    }
}
