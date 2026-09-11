//! Database fixture commands used by repository test scripts.
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use xtask::platform_db::{admin, exit, fingerprint, lock, overlay, suite_db, sweep};

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
    /// One `/proc` pass. Prints a `ZS_SWEEP_*` block for the shim to `eval`.
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
    /// May this run drop what it planned to drop?
    ///
    /// Exit 0 to proceed, 3 (`exit::BLIND`) to refuse. The distinction is the
    /// whole subcommand: "scanned everything, nothing is dead" and "could not
    /// scan, so nothing looked alive" used to be the same zero.
    ///
    /// EVERY INPUT IS A FLAG. The caller gathers them -- the `/proc` half from
    /// `sweep scan`, the rest from the server and from git -- and states what
    /// it found. Nothing here reads the environment, so no ambient value can
    /// turn the refusal off, and there is deliberately no flag that can either.
    Decide {
        /// How many databases the run would drop.
        #[arg(long)]
        doomed: usize,
        /// Did `sweep scan` itself succeed?
        #[arg(long = "proc-scan-ok", action = clap::ArgAction::Set, value_parser = boolish(), default_value = "1")]
        proc_scan_ok: bool,
        /// `ZS_SWEEP_PROC_UNLISTABLE`.
        #[arg(long = "proc-unlistable", action = clap::ArgAction::Set, value_parser = boolish(), default_value = "0")]
        proc_unlistable: bool,
        /// `ZS_SWEEP_PROC_HIDDEN`.
        #[arg(long = "proc-hidden", action = clap::ArgAction::Set, value_parser = boolish(), default_value = "0")]
        proc_hidden: bool,
        /// `ZS_SWEEP_PROC_EXAMINED`.
        #[arg(long = "proc-examined", default_value = "0")]
        proc_examined: usize,
        /// `ZS_SWEEP_PROC_PEER_ENV_READ`.
        #[arg(long = "proc-peer-env-read", default_value = "0")]
        proc_peer_env_read: usize,
        /// `ZS_SWEEP_PROC_UNEXPLAINED`, space separated.
        #[arg(long = "proc-unexplained", default_value = "")]
        proc_unexplained: String,
        /// Did the `pg_stat_activity` query return?
        #[arg(long = "sessions-ok", action = clap::ArgAction::Set, value_parser = boolish(), default_value = "1")]
        sessions_ok: bool,
        /// Did git enumerate the worktrees and the refs?
        #[arg(long = "git-listing-ok", action = clap::ArgAction::Set, value_parser = boolish(), default_value = "1")]
        git_listing_ok: bool,
        /// Working trees `git worktree list` reported.
        #[arg(long = "worktrees-seen", default_value = "0")]
        worktrees_seen: usize,
        /// Working trees whose migration set could be hashed.
        #[arg(long = "worktrees-fingerprinted", default_value = "0")]
        worktrees_fingerprinted: usize,
    },
}

/// `--flag 0` / `--flag 1`, which is what a shell has to hand.
fn boolish() -> clap::builder::BoolishValueParser {
    clap::builder::BoolishValueParser::new()
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
                    );
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
            // The pass's own account of itself, not just its findings. The
            // single `ZS_SWEEP_PROC_UNREADABLE` these replace counted 406 of
            // 495 entries on an ordinary run, so it could never separate a
            // healthy pass from a blind one; `sweep decide` rules on the parts.
            assign("ZS_SWEEP_PROC_EXAMINED", &held.examined.to_string());
            assign("ZS_SWEEP_PROC_VANISHED", &held.vanished.to_string());
            assign("ZS_SWEEP_PROC_ENV_DENIED", &held.env_denied.to_string());
            assign(
                "ZS_SWEEP_PROC_PEER_ENV_READ",
                &held.peer_env_read.to_string(),
            );
            assign(
                "ZS_SWEEP_PROC_UNEXPLAINED",
                &held
                    .unexplained
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            assign(
                "ZS_SWEEP_PROC_UNLISTABLE",
                if held.proc_unlistable { "1" } else { "0" },
            );
            assign(
                "ZS_SWEEP_PROC_HIDDEN",
                if held.proc_hidden { "1" } else { "0" },
            );
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
                // This subcommand answers "which pids hold this name" from a
                // rendered `held_by` body, so the pass's account of itself is
                // not in scope here; `sweep decide` is where that is ruled on.
                ..sweep::Holders::default()
            };
            let pids: Vec<String> = sweep::holders_of(&held, &name)
                .iter()
                .map(i32::to_string)
                .collect();
            print!("{}", pids.join(" "));
            exit::OK
        }
        SweepCmd::Decide {
            doomed,
            proc_scan_ok,
            proc_unlistable,
            proc_hidden,
            proc_examined,
            proc_peer_env_read,
            proc_unexplained,
            sessions_ok,
            git_listing_ok,
            worktrees_seen,
            worktrees_fingerprinted,
        } => {
            let evidence = sweep::Evidence {
                proc_scan_failed: !proc_scan_ok,
                proc_unlistable,
                proc_hidden,
                proc_examined,
                proc_peer_env_read,
                proc_unexplained: proc_unexplained
                    .split_whitespace()
                    .filter_map(|pid| pid.parse().ok())
                    .collect(),
                sessions_query_ok: sessions_ok,
                git_listing_ok,
                worktrees_seen,
                worktrees_fingerprinted,
                doomed,
            };
            match sweep::verdict(&evidence) {
                sweep::Verdict::Proceed => exit::OK,
                // Stdout, not stderr, and exit 0: nothing was at risk, and a
                // warning nobody sees is the silent count this exists to end.
                sweep::Verdict::ProceedWithGaps(gaps) => {
                    print!("{}", sweep::gap_warning_text(&gaps));
                    exit::OK
                }
                sweep::Verdict::Refuse(gaps) => {
                    refuse(&sweep::refusal_text(&gaps, doomed), exit::BLIND)
                }
            }
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
