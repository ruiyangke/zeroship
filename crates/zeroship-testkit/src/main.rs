//! `zs-testkit` -- the harness's database tool.
//!
//! WHY ONE BINARY WITH SUBCOMMANDS, and not `cargo xtask`, a thin binary per
//! helper, or `#[test]` functions:
//!
//!   - The consumers are `.sh` files. `tests/run_auth_suite.sh` sources
//!     `tests/lib/suite_db.sh` and calls `zs_suite_db_provision`; a `#[test]`
//!     cannot be reached from there at all, and a harness nobody can invoke is
//!     worse than the shell it replaced. Those `tests/lib/*.sh` files still
//!     exist and still export the same function names -- their bodies are now
//!     one call to this binary each, so no consumer changed.
//!   - One target rather than eight means one `cargo build` and one path for
//!     the shims to find. Eight thin binaries would each pay their own link.
//!   - No `xtask`: the operator dropped that wrapper deliberately. This is a
//!     normal workspace crate.
//!
//! NOTHING HERE READS THE PROCESS ENVIRONMENT. Every value arrives as an
//! argument or on stdin, including the ones the shell held in exported
//! variables: the shim reads its own environment and passes what it found. A
//! Rust harness that read `TEST_DB` itself would be rebuilding the ambient
//! opt-out the flag-only override exists to prevent -- and the refusal for an
//! ambient `TEST_DB` would then be checking the very channel it was using.
//!
//! OUTPUT CONTRACT. Subcommands that replace a shell function which SET
//! variables print `NAME=value` lines on stdout for the shim to `eval`, with
//! the value single-quoted. Diagnostics go to stderr. Exit codes are the shell
//! library's: 0 yes, 1 no, 2 refused-or-broken.

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use zeroship_testkit::{admin, exit, fingerprint, lock, overlay, suite_db, sweep};

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
    /// Hash the platform migration set a tree would apply.
    #[command(subcommand)]
    Fingerprint(FingerprintCmd),
    /// The branch-keyed suite database.
    #[command(subcommand)]
    SuiteDb(SuiteDbCmd),
    /// The sweeper's verdicts.
    #[command(subcommand)]
    Sweep(SweepCmd),
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
    /// Print the shell assignments `zs_test_config_load` used to export.
    ///
    /// The four values the caller asked for arrive as a `key=value` block on
    /// stdin -- see [`overlay::Wanted::from_block`] for why not argv.
    Load {
        #[arg(long)]
        root: PathBuf,
    },
}

#[derive(Subcommand)]
enum FingerprintCmd {
    /// From a working tree.
    Dir {
        #[arg(long)]
        root: PathBuf,
    },
    /// From a git tree. Prints nothing and exits 1 when the ref has no
    /// migrations, which is an ordinary answer about a ref.
    Ref {
        /// The repository the ref is resolved in. Stated rather than taken
        /// from the working directory, so the caller that enumerated the refs
        /// is the one that says where they live.
        #[arg(long)]
        repo: PathBuf,
        #[arg(long = "ref")]
        reference: String,
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

#[derive(Subcommand)]
enum SweepCmd {
    /// Print the family a database belongs to, or exit 1.
    FamilyOf {
        #[arg(long)]
        name: String,
    },
    /// Is `--pid` `--self-pid` or one of its descendants?
    PidIsOurs {
        #[arg(long = "self-pid")]
        self_pid: i32,
        #[arg(long)]
        pid: i32,
    },
    /// One `/proc` pass. Prints `ZS_SWEEP_HELD_BY=` and
    /// `ZS_SWEEP_PROC_UNREADABLE=` for the shim to `eval`.
    Scan {
        #[arg(long = "self-pid")]
        self_pid: i32,
        /// A file of database names, one per line.
        #[arg(long)]
        patterns: PathBuf,
    },
    /// The pids holding `--name`, read from a `ZS_SWEEP_HELD_BY` body on stdin.
    HoldersOf {
        #[arg(long)]
        name: String,
    },
}

fn main() -> ExitCode {
    let code = match Cli::parse().command {
        Command::Overlay(cmd) => run_overlay(cmd),
        Command::Fingerprint(cmd) => run_fingerprint(cmd),
        Command::SuiteDb(cmd) => run_suite_db(cmd),
        Command::Sweep(cmd) => run_sweep(cmd),
    };
    ExitCode::from(code as u8)
}

fn run_overlay(cmd: OverlayCmd) -> i32 {
    match cmd {
        OverlayCmd::Get { file, section, key } => {
            // A missing file prints nothing and succeeds, which is what the awk
            // did: `zs_test_config_get` is a reader, and the refusal for an
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
            assign("ZS_TEST_REDIS_URL", &loaded.redis_url);
            exit::OK
        }
    }
}

fn run_fingerprint(cmd: FingerprintCmd) -> i32 {
    match cmd {
        FingerprintCmd::Dir { root } => match fingerprint::of_dir(&root) {
            Ok(value) => {
                println!("{value}");
                exit::OK
            }
            Err(refusal) => refuse(&refusal, exit::FATAL),
        },
        FingerprintCmd::Ref { repo, reference } => match fingerprint::of_ref(&repo, &reference) {
            Some(value) => {
                println!("{value}");
                exit::OK
            }
            None => exit::NO,
        },
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
        match std::process::Command::new(&argv[0]).args(&argv[1..]).status() {
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

fn run_sweep(cmd: SweepCmd) -> i32 {
    match cmd {
        SweepCmd::FamilyOf { name } => match sweep::family_of(&name) {
            Some(family) => {
                print!("{family}");
                exit::OK
            }
            None => exit::NO,
        },
        SweepCmd::PidIsOurs { self_pid, pid } => {
            if sweep::pid_is_ours(pid, self_pid) {
                exit::OK
            } else {
                exit::NO
            }
        }
        SweepCmd::Scan { self_pid, patterns } => {
            let body = match std::fs::read_to_string(&patterns) {
                Ok(body) => body,
                Err(why) => {
                    return refuse(
                        &format!("FATAL: could not read {}: {why}\n", patterns.display()),
                        exit::FATAL,
                    )
                }
            };
            let names: Vec<String> = body
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            let held = sweep::scan_holders(&names, self_pid);
            let mut rendered = String::new();
            for (name, pid) in &held.held_by {
                rendered.push_str(&format!("{name} {pid}\n"));
            }
            assign("ZS_SWEEP_HELD_BY", &rendered);
            assign("ZS_SWEEP_PROC_UNREADABLE", &held.unreadable.to_string());
            exit::OK
        }
        SweepCmd::HoldersOf { name } => {
            let body = read_stdin();
            let held = sweep::Holders {
                held_by: body
                    .lines()
                    .filter_map(|line| {
                        let (n, pid) = line.split_once(' ')?;
                        Some((n.to_string(), pid.trim().parse().ok()?))
                    })
                    .collect(),
                unreadable: 0,
            };
            let pids: Vec<String> = sweep::holders_of(&held, &name)
                .iter()
                .map(i32::to_string)
                .collect();
            print!("{}", pids.join(" "));
            exit::OK
        }
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
