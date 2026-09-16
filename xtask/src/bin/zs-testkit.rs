//! Database fixture commands used by repository test scripts.
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use xtask::platform_db::{admin, exit, lock, overlay, suite_db};

#[derive(Parser)]
#[command(
    name = "zs-testkit",
    about = "The zeroship test harness's overlay, suite-database and sweeper logic."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read the generated test overlay, `deploy/ops/zeroship.test.toml`.
    #[command(subcommand)]
    Overlay(OverlayCmd),
    /// The branch-keyed suite database.
    #[command(subcommand)]
    SuiteDb(SuiteDbCmd),
}

#[derive(Subcommand)]
enum OverlayCmd {
    /// Print one `section.key`, or nothing when it is absent.
    Get {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        section: String,
        #[arg(long)]
        key: String,
    },
    /// Print the shell assignments an overlay load exports.
    ///
    /// The four values the caller asked for arrive as a `key=value` block on
    /// stdin -- see [`overlay::Wanted::from_block`] for why not argv.
    Load {
        #[arg(long)]
        root: PathBuf,
    },
}

#[derive(Subcommand)]
enum SuiteDbCmd {
    /// Refuse a name that is not a bare PostgreSQL identifier.
    CheckIdentifier {
        #[arg(long)]
        name: String,
    },
    /// Print `TEST_DB=` and `ZS_SUITE_DB_EXPLICIT=`.
    Resolve {
        #[arg(long)]
        prefix: String,
        /// The `--database <name>` the caller passed, or empty.
        #[arg(long, default_value = "")]
        explicit: String,
        #[arg(long)]
        root: PathBuf,
        /// What the caller found in `TEST_DB`. Passed in rather than read here:
        /// the refusal exists because an ambient variable must not steer a
        /// gate, and reading it directly would make this binary the thing being
        /// steered.
        #[arg(long, default_value = "")]
        ambient_test_db: String,
        /// Likewise for `SKIP_DB_RECREATE`.
        #[arg(long, default_value = "")]
        ambient_skip_db_recreate: String,
    },
    /// 0 present, 1 absent, 2 could not tell.
    Exists {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        name: String,
    },
    /// Create if absent. Never drops, never recreates.
    Ensure {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        name: String,
    },
    /// Ensure, then run a command, both under one per-machine lock.
    Provision {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        name: String,
        /// Where the lock file goes -- the caller's `${TMPDIR:-/tmp}`.
        #[arg(long)]
        lock_dir: PathBuf,
        /// The migrate command, after `--`.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
}

fn main() -> ExitCode {
    let code = match Cli::parse().command {
        Command::Overlay(cmd) => run_overlay(cmd),
        Command::SuiteDb(cmd) => run_suite_db(cmd),
    };
    ExitCode::from(code as u8)
}

fn run_overlay(cmd: OverlayCmd) -> i32 {
    match cmd {
        OverlayCmd::Get { file, section, key } => {
            // A missing file prints nothing and succeeds, which is what the awk
            // did: the `get` is a reader, and the refusal for an
            // absent overlay belongs to `load`, which says what to do about it.
            let document = std::fs::read_to_string(&file).unwrap_or_default();
            if let Some(value) = overlay::get(&document, &section, &key) {
                println!("{value}");
            }
            exit::OK
        }
        OverlayCmd::Load { root } => {
            let loaded = match overlay::load(&root) {
                Ok(loaded) => loaded,
                Err(refusal) => return refuse(&refusal, exit::NO),
            };
            let wanted = overlay::Wanted::from_block(&read_stdin());
            if let Err(refusal) = overlay::assert_agrees(&loaded, &wanted) {
                return refuse(&refusal, exit::NO);
            }
            // Named exactly as the shell exported them: these go into a suite's
            // environment and crates read them there.
            assign("ZS_TEST_OVERLAY", &loaded.overlay.display().to_string());
            assign("PG_HOST", &loaded.host);
            assign("PG_PORT", &loaded.port);
            assign("PG_USER", &loaded.user);
            assign("PG_PASS", &loaded.pass);
            assign("PG_DB", &loaded.db);
            assign("ZS_TEST_PG_DSN", &loaded.dsn);
            exit::OK
        }
    }
}

fn run_suite_db(cmd: SuiteDbCmd) -> i32 {
    match cmd {
        SuiteDbCmd::CheckIdentifier { name } => match suite_db::check_identifier(&name) {
            Ok(()) => exit::OK,
            Err(refusal) => refuse(&refusal, exit::FATAL),
        },
        SuiteDbCmd::Resolve {
            prefix,
            explicit,
            root,
            ambient_test_db,
            ambient_skip_db_recreate,
        } => match suite_db::resolve(
            &prefix,
            &explicit,
            &root,
            &ambient_test_db,
            &ambient_skip_db_recreate,
        ) {
            Ok(resolved) => {
                assign("TEST_DB", &resolved.name);
                assign(
                    "ZS_SUITE_DB_EXPLICIT",
                    if resolved.explicit { "1" } else { "0" },
                );
                exit::OK
            }
            Err(refusal) => refuse(&refusal, exit::FATAL),
        },
        SuiteDbCmd::Exists { root, name } => {
            let mut admin = match connect(&root) {
                Ok(admin) => admin,
                Err(refusal) => return refuse(&refusal, exit::FATAL),
            };
            match admin::DbAdmin::exists(&mut admin, &name) {
                Ok(true) => exit::OK,
                Ok(false) => exit::NO,
                Err(why) => refuse(
                    &format!("FATAL: could not ask the server whether {name} exists ({why}).\n"),
                    exit::FATAL,
                ),
            }
        }
        SuiteDbCmd::Ensure { root, name } => {
            let mut admin = match connect(&root) {
                Ok(admin) => admin,
                Err(refusal) => return refuse(&refusal, exit::FATAL),
            };
            ensure_and_report(&mut admin, &name)
        }
        SuiteDbCmd::Provision {
            root,
            name,
            lock_dir,
            argv,
        } => run_provision(&root, &name, &lock_dir, &argv),
    }
}

fn ensure_and_report(admin: &mut dyn admin::DbAdmin, name: &str) -> i32 {
    let mut say = |line: &str| print!("{line}");
    match suite_db::ensure(admin, name, &mut say) {
        Ok(_) => exit::OK,
        Err(refusal) => refuse(&refusal, exit::FATAL),
    }
}

fn run_provision(root: &Path, name: &str, lock_dir: &Path, argv: &[String]) -> i32 {
    let loaded = match overlay::load(root) {
        Ok(loaded) => loaded,
        Err(refusal) => return refuse(&refusal, exit::FATAL),
    };
    let lock_file = suite_db::lock_path(lock_dir, &loaded.host, &loaded.port, name);
    let server = match admin::Server::from_overlay(&loaded) {
        Ok(server) => server,
        Err(refusal) => return refuse(&refusal, exit::FATAL),
    };

    let held = lock::with_file_lock(&lock_file, suite_db::PROVISION_LOCK_TIMEOUT, || {
        let mut pg = admin::PgAdmin::new(server);
        let code = ensure_and_report(&mut pg, name);
        if code != exit::OK {
            return code;
        }
        // The migrate command runs INSIDE the lock, because the step that races
        // is `CREATE SCHEMA`, which happens before the migrate binary takes its
        // own advisory lock. It is released the moment this closure returns --
        // never held over the tests, which run for tens of minutes.
        match std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
        {
            Ok(status) => status.code().unwrap_or(exit::FATAL),
            Err(why) => refuse(
                &format!("FATAL: could not run {}: {why}\n", argv[0]),
                exit::FATAL,
            ),
        }
    });

    match held {
        Ok(code) => code,
        Err(lock::LockError::TimedOut) => refuse(
            &suite_db::provision_lock_timeout_message(name, &lock_file),
            exit::FATAL,
        ),
        // NOT the timeout message. A lock directory that does not exist would
        // otherwise be reported as a peer holding the lock for fifteen minutes,
        // and the caller would go looking for a run that was never there.
        Err(lock::LockError::Unusable(why)) => refuse(
            &format!(
                "FATAL: could not take the provisioning lock at {}: {why}\n",
                lock_file.display()
            ),
            exit::FATAL,
        ),
    }
}

/// Open a connection to the server the overlay under `root` names.
fn connect(root: &Path) -> Result<admin::PgAdmin, String> {
    let loaded = overlay::load(root)?;
    Ok(admin::PgAdmin::new(admin::Server::from_overlay(&loaded)?))
}

/// Print one `NAME='value'` line for the shim to `eval`.
///
/// Single quotes with the `'\''` escape: the values here include a DSN with a
/// password in it, and a shim that `eval`ed an unquoted assignment would let a
/// space or a `$` in one become shell syntax.
fn assign(name: &str, value: &str) {
    println!("{name}='{}'", value.replace('\'', "'\\''"));
}

/// Write a refusal to stderr and hand back the exit code, so call sites read
/// `return refuse(...)` rather than two statements that could drift apart.
fn refuse(message: &str, code: i32) -> i32 {
    eprint!("{message}");
    code
}

fn read_stdin() -> String {
    use std::io::Read;
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}
