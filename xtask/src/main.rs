mod auth;
mod billing;
mod data;
mod migrations;
mod storage;
mod worker;
mod workflow;

#[path = "../../tests/fixtures/postgres/image.rs"]
mod postgres_image;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Parser)]
#[command(about = "Repository development tasks")]
struct Args {
    #[command(subcommand)]
    command: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Run native and example test suites with owned backing services.
    Test {
        #[command(subcommand)]
        suite: Suite,
    },
    /// Reclaim the test databases no branch can still need.
    #[command(subcommand)]
    PlatformDb(PlatformDbCmd),
}

#[derive(Subcommand)]
enum PlatformDbCmd {
    /// Decide which suite databases are unreachable, and optionally drop them.
    Sweep {
        /// Actually drop. Without it the plan is printed and nothing is destroyed.
        #[arg(long)]
        apply: bool,
        /// Also consider pre-hash names, whose reachability cannot be decided.
        #[arg(long)]
        legacy: bool,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<String>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password: Option<String>,
    },
}

#[derive(Subcommand)]
enum Suite {
    /// Run auth, authorization, mailer and gateway tests with owned services.
    Auth,
    /// Run billing, migration and stream tests with owned services.
    Billing,
    /// Run worker tests with owned `PostgreSQL` and Redis fixtures.
    Worker,
    /// Build the Node host and test the platform corpus on owned PostgreSQL.
    Migrations,
    /// Check workspace dependency and feature declarations.
    Repository,
    /// Run workflow crates, runtime, control-plane, SDK and example tests.
    Workflow,
    /// Run storage crate tests and the examples' Vitest/Playwright suites.
    Storage,
    /// Check data crate boundaries and database deployment/test posture.
    DataArchitecture,
    /// Run the data crates against PostgreSQL, SQLite files and the CDC relay.
    Data {
        /// Select tests for a diagnostic run; database setup remains mandatory.
        #[arg(long)]
        filter: Option<String>,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    if let Err(error) = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::Relaxed)) {
        eprintln!("cannot install test cleanup signal handler: {error}");
        return ExitCode::FAILURE;
    }
    let result = match args.command {
        Task::PlatformDb(PlatformDbCmd::Sweep {
            apply,
            legacy,
            host,
            port,
            user,
            password,
        }) => {
            // The sweeper's exit codes ARE its contract - 2 is "could not look",
            // 3 is "incomplete evidence and something to destroy" - and
            // `Result<()>` collapses them all to 1. So this arm leaves through
            // the same door `main` would have, with the code intact.
            let code = xtask::platform_db::sweep_command::run(
                &root(),
                &xtask::platform_db::sweep_command::Args {
                    apply,
                    legacy,
                    host,
                    port,
                    user,
                    password,
                },
            );
            return ExitCode::from(u8::try_from(code).unwrap_or(1));
        }
        Task::Test { suite: Suite::Auth } => auth::run(),
        Task::Test {
            suite: Suite::Billing,
        } => billing::run(),
        Task::Test {
            suite: Suite::Worker,
        } => worker::run(),
        Task::Test {
            suite: Suite::Migrations,
        } => migrations::run(),
        Task::Test {
            suite: Suite::Repository,
        } => checked(
            cargo().args([
                "test",
                "--manifest-path",
                "xtask/Cargo.toml",
                "--test",
                "repository_architecture",
            ]),
            "repository architecture",
        )
        // This area owns repository-wide invariants, so the harness's own unit
        // tests belong to it. `xtask` declares a separate workspace, which puts
        // it outside every `--workspace` command run from the root, and its
        // binary target is reached by no other area.
        .and_then(|()| {
            checked(
                cargo().args([
                    "test",
                    "--manifest-path",
                    "xtask/Cargo.toml",
                    "--bin",
                    "xtask",
                ]),
                "harness unit tests",
            )
        }),
        Task::Test {
            suite: Suite::Workflow,
        } => workflow::run(),
        Task::Test {
            suite: Suite::Storage,
        } => storage::run(),
        Task::Test {
            suite: Suite::DataArchitecture,
        } => data::architecture(),
        Task::Test {
            suite: Suite::Data { filter },
        } => data::run(filter.as_deref()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tests failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the repository")
        .to_owned()
}

/// A cargo invocation, and the test binaries it goes on to spawn.
///
/// libtest gives every test its own thread, and a spawned thread's default
/// stack is a fraction of the main thread's. A test deep enough to exhaust it
/// does not fail: it aborts the process, libtest never prints a result line,
/// and a summary that counts those lines scores the aborted target as zero
/// failures. Raising the floor here covers every area, because a value set per
/// area is absent from the next area someone adds.
fn cargo() -> Command {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(root());
    command.env("RUST_MIN_STACK", "33554432");
    command
}

fn checked(command: &mut Command, description: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    cancelled()?;
    let mut child = command
        .process_group(0)
        .spawn()
        .map_err(|error| format!("{description}: {error}"))?;
    let mut interrupt = None;
    loop {
        if let Some(status) = child.try_wait()? {
            cancelled()?;
            if !status.success() {
                return Err(format!("{description}: {status}").into());
            }
            return Ok(());
        }
        if INTERRUPTED.load(Ordering::Relaxed) {
            if let Some(started) = interrupt {
                // Outlast nextest's shutdown grace period so its fixture
                // watchdogs can finish removing containers before we reap it.
                if Instant::now().duration_since(started) > Duration::from_secs(60) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("test run interrupted".into());
                }
            } else {
                // Give nextest time to stop its tests and their service children.
                let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id())?);
                let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGINT);
                interrupt = Some(Instant::now());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn cancelled() -> Result<()> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        Err("test run interrupted".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::cargo;
    use std::ffi::OsStr;

    /// Every area runs its suites through [`cargo`], so a stack floor set here
    /// reaches all of them and one set beside a single area reaches only that
    /// area. The floor has to be a raise: a spawned thread's default stack is a
    /// fraction of the main thread's, and a test that exhausts it aborts the
    /// process rather than failing, which reports as no failures at all.
    #[test]
    fn cargo_raises_the_thread_stack_floor_for_spawned_tests() {
        let command = cargo();
        let floor = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("RUST_MIN_STACK"))
            .and_then(|(_, value)| value)
            .expect("cargo() must set RUST_MIN_STACK for the test binaries it spawns");
        let bytes: usize = floor
            .to_str()
            .expect("the stack floor must be UTF-8")
            .parse()
            .expect("the stack floor must be a plain byte count");
        assert!(
            bytes >= 8 << 20,
            "the stack floor must RAISE the default rather than merely be present, got {bytes}"
        );
    }
}
