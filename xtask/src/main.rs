mod auth;
mod billing;
mod data;
mod migrations;
mod storage;
mod worker;
mod workflow;

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
    /// Check the workspace's Playwright against the development shell's
    /// browsers. Needs Nix, `pnpm install` and the shell's browsers built.
    PlaywrightBrowsers,
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
            suite: Suite::PlaywrightBrowsers,
        } => checked(
            cargo().args([
                "test",
                "--manifest-path",
                "xtask/Cargo.toml",
                "--test",
                "playwright_browsers",
            ]),
            "playwright browsers",
        ),
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
/// Runs from [`root`] so cargo reads the workspace's `.cargo/config.toml`,
/// which is where the spawned tests' stack floor comes from. See
/// [`the_workspace_config_raises_the_thread_stack_floor`]. xtask itself runs
/// under `cargo run`, so the command leaves out the variables describing the
/// xtask package; [`nested_cargo`] says why.
fn cargo() -> Command {
    let mut command = zeroship_testkit::nested_cargo::cargo();
    command.current_dir(root());
    command
}

/// A package script in `directory`, run by Node rather than by a package
/// manager.
///
/// A package manager run is not an exec. It first brings the whole workspace's
/// `node_modules` up to date, and [`root`] is baked in when xtask is compiled,
/// so an xtask built inside a git worktree aims that install at the worktree. A
/// worktree's `node_modules` are links into the primary checkout, which is where
/// the install lands: it repoints the primary checkout's package links at the
/// worktree's virtual store, and every checkout is then unable to load what it
/// linked. `node --run` reads the package's own `package.json`, puts that
/// package's `node_modules/.bin` on `PATH`, and execs the script. It resolves no
/// workspace, installs nothing, and writes into no other project.
///
/// The script must not reach for a package manager either, or the install
/// returns one level down. Each area asserts that through
/// [`script_contract::assert_no_package_manager`].
fn script(directory: &str, name: &str) -> Command {
    let mut command = Command::new("node");
    command
        .current_dir(root().join(directory))
        .args(["--run", name]);
    command
}

/// What a package script xtask names may not do.
#[cfg(test)]
pub(crate) mod script_contract {
    use super::{root, script};

    /// The programs that bring a whole workspace up to date before running
    /// anything. One inside a script puts that install back underneath
    /// `node --run`, one level down, where it lands in whichever checkout the
    /// links resolve to.
    const PACKAGE_MANAGERS: [&str; 5] = ["pnpm", "npm", "npx", "yarn", "bun"];

    /// Assert the invocation xtask builds for `(directory, name)` runs the
    /// declared script under Node, in that package, and that the script it will
    /// run needs no package manager of its own.
    pub(crate) fn assert_no_package_manager(directory: &str, package: &str, name: &str) {
        let command = script(directory, name);
        assert_eq!(
            command.get_program(),
            "node",
            "{directory}'s {name} must run under Node, not a package manager"
        );
        let arguments: Vec<_> = command.get_args().collect();
        assert_eq!(
            arguments,
            ["--run", name],
            "{directory}'s {name} must be the script Node is asked to run"
        );
        let workspace = root().join(directory);
        assert_eq!(
            command.get_current_dir(),
            Some(workspace.as_path()),
            "a script runs in the package that declares it"
        );

        let manifest = workspace.join("package.json");
        let text = std::fs::read_to_string(&manifest)
            .unwrap_or_else(|error| panic!("read {}: {error}", manifest.display()));
        let manifest: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("parse {directory}/package.json: {error}"));
        assert_eq!(
            manifest["name"].as_str(),
            Some(package),
            "{directory} declares a different package than xtask names"
        );
        let scripts = &manifest["scripts"];
        let body = scripts[name]
            .as_str()
            .unwrap_or_else(|| panic!("{directory} declares no {name} script"));
        assert!(
            !body.trim().is_empty(),
            "{directory}'s {name} script is empty, so it asserts nothing"
        );
        for hook in [format!("pre{name}"), format!("post{name}")] {
            assert!(
                scripts[&hook].is_null(),
                "`node --run` does not run {hook}, so {directory} would silently \
                 stop running it; fold it into {name} or call it separately"
            );
        }
        let mut programs = 0;
        for token in
            body.split(|character: char| character.is_whitespace() || "&|;()".contains(character))
        {
            let program = token.rsplit('/').next().unwrap_or(token);
            if program.is_empty() {
                continue;
            }
            programs += 1;
            assert!(
                !PACKAGE_MANAGERS.contains(&program),
                "{directory}'s {name} script runs {program}, which installs the \
                 whole workspace before it runs anything; reach the sub-script \
                 with `node --run` instead"
            );
        }
        assert!(
            programs > 0,
            "{directory}'s {name} script yielded no words to check"
        );
    }
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
    use super::{cargo, root};

    /// The cargo xtask spawns leaves out the variables cargo gave the running
    /// binary about its own package, and keeps the rest of the environment.
    ///
    /// `cargo test` gives this test binary the xtask package's variables, the
    /// same ones `cargo run` gives xtask. The three names asserted as removed
    /// are ones cargo sets for every binary it runs, and a name is removed only
    /// if it is present, so an environment without them fails here rather than
    /// passing over nothing. `CARGO` is the control: cargo sets it beside
    /// them, it shares their prefix, and the spawned cargo keeps it.
    #[test]
    fn a_spawned_cargo_leaves_out_the_package_variables_and_keeps_the_rest() {
        let command = cargo();
        let changed: Vec<(String, bool)> = command
            .get_envs()
            .map(|(name, value)| (name.to_string_lossy().into_owned(), value.is_some()))
            .collect();
        assert!(
            changed.iter().all(|(_, set)| !set),
            "the spawned cargo only removes variables, never sets one: {changed:?}"
        );
        let removed: std::collections::BTreeSet<&str> =
            changed.iter().map(|(name, _)| name.as_str()).collect();
        for name in ["CARGO_MANIFEST_DIR", "CARGO_PKG_NAME", "CARGO_PKG_VERSION"] {
            assert!(
                removed.contains(name),
                "{name} describes the running package and must not reach the spawned \
                 cargo; removed: {removed:?}"
            );
        }
        for name in &removed {
            assert!(
                *name == "OUT_DIR"
                    || ["CARGO_PKG_", "CARGO_MANIFEST_", "CARGO_BIN_EXE_"]
                        .iter()
                        .any(|prefix| name.starts_with(prefix)),
                "{name} describes no package and must reach the spawned cargo"
            );
        }
        assert!(
            !removed.contains("CARGO"),
            "CARGO names the cargo binary, not a package, and must be kept"
        );
    }

    /// The floor is asserted where it is SET rather than where one caller
    /// passes it on, so it covers every cargo invocation: a bare `cargo test`
    /// carries it as much as `cargo xtask` or CI, and a floor set beside one
    /// caller is absent from the next caller someone adds.
    ///
    /// It has to be a RAISE rather than merely present. A spawned thread's
    /// default stack is a fraction of the main thread's, and a test that
    /// exhausts it ABORTS the process rather than failing - libtest prints no
    /// result line, so a summary counting those lines scores the aborted
    /// target as zero failures.
    #[test]
    fn the_workspace_config_raises_the_thread_stack_floor() {
        let path = root().join(".cargo").join("config.toml");
        let text = std::fs::read_to_string(&path)
            .expect("the workspace cargo config must exist");
        let config: toml_edit::DocumentMut = text
            .parse()
            .expect("the workspace cargo config must parse as TOML");
        let floor = config
            .get("env")
            .and_then(|env| env.get("RUST_MIN_STACK"))
            .and_then(|entry| entry.get("value"))
            .and_then(|value| value.as_str())
            .expect("[env].RUST_MIN_STACK must carry a string byte count");
        let bytes: usize = floor
            .parse()
            .expect("the stack floor must be a plain byte count");
        assert!(
            bytes >= 8 << 20,
            "the stack floor must RAISE the default rather than merely be present, got {bytes}"
        );
    }
}
