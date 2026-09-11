mod data;

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
    /// Run native test suites with owned backing services.
    Test {
        #[command(subcommand)]
        suite: Suite,
    },
}

#[derive(Subcommand)]
enum Suite {
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
            eprintln!("data tests failed: {error}");
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

fn cargo() -> Command {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(root());
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
