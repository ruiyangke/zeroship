//! `zeroship-migrate` — the migration CLI (design §9, Phase 3; Track A, Phase A3).
//!
//! A dbmate-like, generic-by-default migration tool. `main` is a THIN arg-parser:
//! it parses flags with `clap`, builds a
//! [`RunConfig`](zeroship_migrate::guard::platform_runner::RunConfig)-equivalent,
//! and delegates to the `guard::platform_runner::run_*` functions — the ONLY place
//! the `OperatorCapability` token is minted (the §5 trust invariant). `main` never
//! touches the guard internals or mints a capability itself.
//!
//! Command parity with dbmate: `new`, `up` (alias of `migrate`), `down`, `migrate`,
//! `rollback`, `status`, `validate`, `wait`, `dump`. `load` is intentionally NOT
//! implemented (see the `Dump`/`load` note below).
//!
//! Trust posture: the public tool defaults to `--profile trusted` (the operator
//! owns the DB — no zeroship deny-list, no schema allowlist). `--profile platform`
//! / `--profile confined` remain EXPLICIT opt-ins. The CLI's `--profile trusted`
//! flag is the ONLY surface that reaches `RunProfile::Trusted`; the control plane
//! uses `submit_migration` (Confined) and never reaches this binary.
//!
//! compio, ZERO tokio: `#[compio::main]` drives the same compio-native `connect`
//! + executor the library uses.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, ExitCode};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

// The runner module is `pub(crate)`; the bin is part of the same crate so it can
// reach it. External crates cannot — the token mint stays confined.
use zeroship_migrate::guard::platform_runner::{
    self, default_platform_extensions, default_platform_schemas, Engine, RunConfig, RunError,
    RunProfile, RunReport,
};
use zeroship_migrate::loader::new_dbmate_migration;

/// The generic-by-default migration directory for the public tool (dbmate parity).
const DEFAULT_DIR: &str = "./db/migrations";

/// The generic meta/journal schema for the public (Trusted/Confined) tool —
/// dbmate's `schema_migrations` lives in `public`. The Platform profile overrides
/// this to `<primary-schema>_migrations` (e.g. `zeroship_migrations`).
const DEFAULT_GENERIC_SCHEMA: &str = "public";

/// dbmate's conventional schema-dump output path.
const DEFAULT_SCHEMA_FILE: &str = "./db/schema.sql";

/// Migration CLI for Postgres — a dbmate-like tool, generic by default.
#[derive(Debug, Parser)]
#[command(name = "zeroship-migrate", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Migration directory. dbmate files (`<14-digit>_<desc>.sql` with
    /// `-- migrate:up`/`down` sections) or Flyway files (`V<NNNN>__*.sql`) are
    /// auto-detected. Default `./db/migrations`.
    #[arg(long, global = true, default_value = DEFAULT_DIR)]
    dir: PathBuf,

    /// Postgres DSN. Falls back to the `DATABASE_URL` env var.
    #[arg(long, global = true, env = "DATABASE_URL")]
    database_url: Option<String>,

    /// Trust profile. `trusted` (DEFAULT — the public posture: the operator owns
    /// the DB, so the deny-list is OFF and there is no schema confinement);
    /// `platform` widens the guard for the zeroship platform schemas (the
    /// internal compose/ops posture — must be passed EXPLICITLY); `confined` is
    /// the full creator deny-list, single-schema.
    #[arg(long, global = true, value_enum, default_value_t = ProfileArg::Trusted)]
    profile: ProfileArg,

    /// Schema allowlist for the `platform` profile (repeatable). Default:
    /// `zeroship`, `oauth_hydra`, `public`. The FIRST value is the primary schema
    /// pinned into `search_path`. Ignored under `trusted` (no confinement).
    #[arg(long = "schema", global = true)]
    schema: Vec<String>,

    /// The `CREATE EXTENSION` allowlist for the `platform` profile (repeatable).
    /// Default: `citext`, `uuid-ossp`. Ignored under `trusted`.
    #[arg(long = "extension", global = true)]
    extension: Vec<String>,

    /// The meta schema holding the append-only journal. Default: `public` under
    /// `trusted`/`confined` (dbmate's `schema_migrations` lives in `public`),
    /// `<primary-schema>_migrations` under `platform`.
    #[arg(long, global = true)]
    meta_schema: Option<String>,

    /// The advisory-lock serialization sentinel + journal project id. Two
    /// concurrent runs hash to the same `pg_advisory_lock(hashtext(project_id))`
    /// and serialize (§9). Default `default` (generic); the compose/ops platform
    /// path passes `platform`.
    #[arg(long, global = true, default_value = "default")]
    project_id: String,

    /// Approve a DESTRUCTIVE plan (required by `migrate`/`up` when the plan is
    /// destructive, and ALWAYS by `rollback`/`down`). `--allow-destructive` is an
    /// alias.
    #[arg(long, global = true, visible_alias = "allow-destructive")]
    yes: bool,

    /// Print non-blocking operational advisories (lock-heavy ops, drops, missing
    /// indexes) per migration. The value-add for the generic/Trusted mode where the
    /// deny-list is off. Wired into `migrate`/`up`/`validate` output. Never denies.
    #[arg(long, global = true)]
    lint: bool,

    /// Per-statement timeout, seconds.
    #[arg(long, global = true, default_value_t = 60)]
    statement_timeout_secs: u64,

    /// Per-statement lock-acquisition timeout, seconds.
    #[arg(long, global = true, default_value_t = 30)]
    lock_timeout_secs: u64,
}

/// The CLI trust-profile flag value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ProfileArg {
    /// The public dbmate-like posture (the binary default): deny-list OFF, no
    /// schema confinement. The operator owns the DB.
    Trusted,
    /// The widened zeroship platform guard (explicit; the compose/ops posture).
    Platform,
    /// The full creator deny-list, single-schema.
    Confined,
}

impl From<ProfileArg> for RunProfile {
    fn from(p: ProfileArg) -> Self {
        match p {
            ProfileArg::Trusted => Self::Trusted,
            ProfileArg::Platform => Self::Platform,
            ProfileArg::Confined => Self::Confined,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a new dbmate-format migration file (`<14-digit-ts>_<name>.sql`) in
    /// `--dir`, with empty `-- migrate:up`/`down` sections. Prints the created path.
    New {
        /// The migration description (`[A-Za-z0-9_]+` by convention).
        name: String,
    },
    /// Apply all pending migrations. Requires `--yes`/`--allow-destructive` when
    /// the plan is destructive.
    Migrate,
    /// Alias of `migrate`: apply all pending migrations (dbmate `up`).
    Up,
    /// Roll back the SINGLE most-recently-applied migration (dbmate `down`).
    /// GATED on `--yes` (rollback is destructive).
    Down,
    /// Print applied vs pending (+ rolled-back). Reads the journal; no DDL.
    Status,
    /// Dry-run on a shadow DB + report checksum drift + destructive advisories.
    /// NO DDL on the real DB.
    Validate,
    /// Roll back applied migrations via their `down` (gated; requires `--yes`).
    /// `--to <version>` unwinds everything after that numeric version; `--steps
    /// <N>` unwinds the N most-recent. Neither ⇒ roll back ALL.
    Rollback {
        /// Roll back everything strictly AFTER this numeric file version.
        #[arg(long, conflicts_with = "steps")]
        to: Option<u64>,
        /// Roll back the N most-recently-applied migrations.
        #[arg(long)]
        steps: Option<usize>,
    },
    /// Poll the DB until it accepts a connection (a `SELECT 1`), or time out
    /// (dbmate `wait`). Exit 0 when ready, non-zero on timeout.
    Wait {
        /// Timeout budget, seconds.
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,
    },
    /// Dump the DB schema to a file, with a trailer listing the journal's applied
    /// versions (dbmate `dump`). Engine-agnostic: Postgres shells
    /// `pg_dump --schema-only`; SQLite derives the CREATE statements from
    /// `sqlite_master` (no `sqlite3` shell-out). Both write the same trailer.
    Dump {
        /// Output path. Default `./db/schema.sql`.
        #[arg(long, default_value = DEFAULT_SCHEMA_FILE)]
        schema_file: PathBuf,
    },
}

/// Build the runner [`RunConfig`] from the parsed CLI args (defaults applied here,
/// per §9). `main` stays thin: parse → this → delegate.
fn run_config(cli: &Cli) -> Result<RunConfig, String> {
    let database_url = cli
        .database_url
        .clone()
        .ok_or_else(|| "missing --database-url (or DATABASE_URL env)".to_string())?;

    let profile: RunProfile = cli.profile.into();

    // Schema allowlist + primary schema depend on the profile:
    // - Platform: the zeroship allowlist (or the explicit --schema list).
    // - Trusted / Confined (generic): a single schema (default `public`).
    let (schemas, project_schema) = match profile {
        RunProfile::Platform => {
            let schemas = if cli.schema.is_empty() {
                default_platform_schemas()
            } else {
                cli.schema.clone()
            };
            let primary = schemas
                .first()
                .cloned()
                .ok_or_else(|| "schema allowlist is empty".to_string())?;
            (schemas, primary)
        }
        RunProfile::Trusted | RunProfile::Confined => {
            // Generic: the operator's own DB. A single schema (the first --schema,
            // or the dbmate-ish `public` default). No cross-schema allowlist.
            let primary = cli
                .schema
                .first()
                .cloned()
                .unwrap_or_else(|| DEFAULT_GENERIC_SCHEMA.to_string());
            (vec![primary.clone()], primary)
        }
    };
    let extensions = if cli.extension.is_empty() {
        default_platform_extensions()
    } else {
        cli.extension.clone()
    };
    // The meta/journal schema: explicit --meta-schema wins; else a generic
    // `public` for trusted/confined (dbmate's `schema_migrations` home) and
    // `<primary>_migrations` for platform.
    let meta_schema = cli.meta_schema.clone().unwrap_or_else(|| match profile {
        RunProfile::Platform => format!("{project_schema}_migrations"),
        RunProfile::Trusted | RunProfile::Confined => DEFAULT_GENERIC_SCHEMA.to_string(),
    });

    Ok(RunConfig {
        dir: cli.dir.clone(),
        database_url,
        profile,
        project_id: cli.project_id.clone(),
        project_schema,
        schemas,
        extensions,
        meta_schema,
        yes: cli.yes,
        statement_timeout: Duration::from_secs(cli.statement_timeout_secs),
        lock_timeout: Duration::from_secs(cli.lock_timeout_secs),
    })
}

/// Format a 14-digit `YYYYMMDDHHMMSS` timestamp from a [`SystemTime`].
///
/// This is the CLI/bin (std time is fine — only workflow SCRIPTS forbid clocks).
/// Computed from the Unix epoch via civil-from-days (no external date crate); UTC.
fn timestamp_14(now: SystemTime) -> String {
    let secs: i64 = now
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}{month:02}{day:02}{hh:02}{mm:02}{ss:02}")
}

/// Convert a day count since the Unix epoch into a `(year, month, day)` civil date
/// (Howard Hinnant's `civil_from_days`, public-domain algorithm). UTC, proleptic
/// Gregorian. Avoids a date-crate dependency for the one timestamp `new` needs.
/// All arithmetic is `i64`; the result tuple is `(year, month, day)`.
const fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Print non-blocking advisories for a set of `(version, advisories)` pairs (the
/// `--lint` output). Header-then-lines; never affects the exit code.
fn print_lint(pairs: &[(String, Vec<zeroship_migrate::analyze::Advisory>)]) {
    let total: usize = pairs.iter().map(|(_, a)| a.len()).sum();
    if total == 0 {
        println!("lint: no advisories");
        return;
    }
    println!("lint: {total} advisor(y/ies)");
    for (version, advs) in pairs {
        for a in advs {
            print!("  [{version}] {:?} {}: {}", a.severity, a.rule, a.message);
            if let Some(s) = &a.suggestion {
                print!(" (suggest: {s})");
            }
            println!();
        }
    }
}

/// Print the runner's report to stdout in a compact, operator-readable form.
fn print_report(report: &RunReport) {
    match report {
        RunReport::Migrate(outcome) => {
            if outcome.is_noop() {
                println!("migrate: no-op (everything already applied)");
            } else {
                println!(
                    "migrate: applied {} ({:?}), recovered {}, skipped {}",
                    outcome.applied.len(),
                    outcome.applied,
                    outcome.recovered.len(),
                    outcome.skipped.len()
                );
            }
        }
        RunReport::Status(st) => {
            let current = st
                .current_version
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |v| v.as_str().to_string());
            println!(
                "status: current={current} applied={} pending={} rolled_back={}",
                st.applied.len(),
                st.pending.len(),
                st.rolled_back.len()
            );
            for e in &st.applied {
                println!("  applied  {} ({:?})", e.version, e.phase);
            }
            for v in &st.pending {
                println!("  pending  {}", v.as_str());
            }
            for rb in &st.rolled_back {
                println!("  rolled-back {}", rb.version);
            }
        }
        RunReport::Validate(v) => {
            println!(
                "validate: dry-run ok={} destructive={} drift_clean={}",
                v.dry_run.ok,
                v.destructive,
                v.drift.is_clean()
            );
            for (version, advs) in &v.advisories {
                for a in advs {
                    println!("  advisory [{version}] {:?}: {}", a.severity, a.message);
                }
            }
            if !v.drift.is_clean() {
                for d in &v.drift.checksum_drift {
                    println!("  DRIFT {} (journal != set)", d.version);
                }
                for o in &v.drift.orphan_journal {
                    println!("  ORPHAN {} (in journal, not in set)", o.version);
                }
            }
        }
        RunReport::Rollback(outcome) => {
            println!(
                "rollback: rolled_back {:?} skipped_irreversible {:?}",
                outcome.rolled_back, outcome.skipped_irreversible
            );
        }
    }
}

/// `new` — generate a dbmate-format migration file in `--dir`. Errors if the file
/// already exists (never clobbers). Prints the created path.
fn run_new(cli: &Cli, name: &str) -> Result<(), String> {
    let stamp = timestamp_14(SystemTime::now());
    let (filename, contents) = new_dbmate_migration(&stamp, name);
    let path = cli.dir.join(&filename);
    if path.exists() {
        return Err(format!("refusing to overwrite existing file: {}", path.display()));
    }
    std::fs::create_dir_all(&cli.dir)
        .map_err(|e| format!("create dir {}: {e}", cli.dir.display()))?;
    std::fs::write(&path, contents).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!("Creating migration: {}", path.display());
    Ok(())
}

/// The Postgres `dump` schema body: shell `pg_dump --schema-only` against the DSN.
/// Find it on `PATH` (or honour `PG_DUMP` for a pinned binary, e.g. under Nix where
/// it is not on `PATH`). If `pg_dump` is unavailable / fails, error (don't
/// half-dump). Byte-identical to the pre-multi-engine PG dump body.
fn dump_schema_pg(database_url: &str) -> Result<String, String> {
    let pg_dump = std::env::var("PG_DUMP").unwrap_or_else(|_| "pg_dump".to_string());
    let output = ProcCommand::new(&pg_dump)
        .arg("--schema-only")
        .arg("--no-owner")
        .arg("--no-privileges")
        .arg(database_url)
        .output()
        .map_err(|e| {
            format!("could not run `{pg_dump}` (is it on PATH? set PG_DUMP to a path): {e}")
        })?;
    if !output.status.success() {
        return Err(format!(
            "pg_dump failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `dump` — derive the DB schema (engine-agnostic), then append a trailer listing
/// the journal's applied versions (dbmate `dump`). Postgres shells
/// `pg_dump --schema-only`; SQLite derives the CREATE statements from
/// `sqlite_master` through the hardened backend (no `sqlite3` shell-out). Both
/// engines write the SAME applied-versions trailer, so `schema.sql` is one
/// consistent contract. An unsupported engine is an honest refusal.
async fn run_dump(cfg: &RunConfig, schema_file: &Path) -> Result<(), String> {
    // The schema body is engine-specific; the trailer is shared (below).
    let mut schema = match platform_runner::classify_engine(&cfg.database_url) {
        Engine::Postgres => dump_schema_pg(&cfg.database_url)?,
        Engine::Sqlite(app) => platform_runner::dump_schema_sqlite(&app)
            .await
            .map_err(|e| e.to_string())?,
        Engine::Unsupported => return Err(RunError::UnsupportedEngine.to_string()),
    };

    // Append the applied-versions trailer (dbmate writes the schema_migrations
    // rows so a fresh `db:setup` knows which versions are already applied). We read
    // the journal via the runner's status view. Identical shape for BOTH engines.
    let versions = applied_versions(cfg).await?;
    schema.push_str("\n-- zeroship-migrate schema_migrations\n");
    for v in &versions {
        let _ = writeln!(schema, "--   {v}");
    }

    if let Some(parent) = schema_file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(schema_file, schema)
        .map_err(|e| format!("write {}: {e}", schema_file.display()))?;
    println!("Dumped schema to {}", schema_file.display());
    Ok(())
}

/// Read the applied migration versions from the journal (for the `dump` trailer),
/// via the runner's `status` view. Returns the version strings in applied order.
async fn applied_versions(cfg: &RunConfig) -> Result<Vec<String>, String> {
    match platform_runner::run_status(cfg).await {
        Ok(RunReport::Status(st)) => Ok(st
            .applied
            .iter()
            .map(|e| e.version.as_str().to_string())
            .collect()),
        Ok(_) => Ok(Vec::new()),
        Err(e) => Err(format!("read journal for dump trailer: {e}")),
    }
}

#[compio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // `new` is offline (no DB): handle it before building a DSN-bearing RunConfig.
    if let Command::New { name } = &cli.command {
        return match run_new(&cli, name) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("zeroship-migrate: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let cfg = match run_config(&cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zeroship-migrate: {e}");
            return ExitCode::FAILURE;
        }
    };

    // `wait` polls the DSN; no migration load. Handle it directly.
    if let Command::Wait { timeout_secs } = &cli.command {
        let res = platform_runner::run_wait(
            &cfg.database_url,
            Duration::from_secs(*timeout_secs),
            Duration::from_millis(500),
        )
        .await;
        return match res {
            Ok(()) => {
                println!("wait: database is ready");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("zeroship-migrate: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // `dump` shells pg_dump; no migration plan.
    if let Command::Dump { schema_file } = &cli.command {
        return match run_dump(&cfg, schema_file).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("zeroship-migrate: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let result = match &cli.command {
        Command::Migrate | Command::Up => platform_runner::run_migrate(&cfg).await,
        Command::Down => platform_runner::run_down(&cfg).await,
        Command::Status => platform_runner::run_status(&cfg).await,
        Command::Validate => platform_runner::run_validate(&cfg).await,
        Command::Rollback { to, steps } => {
            platform_runner::run_rollback(&cfg, *to, *steps).await
        }
        // Handled above (offline / non-plan commands).
        Command::New { .. } | Command::Wait { .. } | Command::Dump { .. } => unreachable!(),
    };

    match result {
        Ok(report) => {
            print_report(&report);
            // `--lint`: opt-in, non-blocking advisories for migrate/up/validate.
            if cli.lint && matches!(cli.command, Command::Migrate | Command::Up | Command::Validate)
            {
                match platform_runner::lint_advisories(&cfg) {
                    Ok(pairs) => print_lint(&pairs),
                    Err(e) => eprintln!("zeroship-migrate: lint: {e}"),
                }
            }
            // `validate` is a GATE: a failing shadow dry-run or detected checksum
            // drift must FAIL the process so CI can block on it — `validate` exits 0
            // ONLY on a fully clean validate. The printed lines are unchanged
            // (`print_report` already emitted `dry-run ok=… drift_clean=…`); only the
            // process exit code reflects the verdict. Applies uniformly to both the
            // PG and SQLite legs (the verdict is read off the engine-agnostic
            // `ValidateReport`). Every other command keeps its prior exit semantics.
            if let RunReport::Validate(v) = &report {
                if !v.dry_run.ok || !v.drift.is_clean() {
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zeroship-migrate: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("args parse")
    }

    #[test]
    fn profile_defaults_to_trusted() {
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        assert_eq!(cli.profile, ProfileArg::Trusted);
        assert_eq!(RunProfile::from(cli.profile), RunProfile::Trusted);
    }

    #[test]
    fn profile_arg_trusted_maps_to_run_profile_trusted() {
        assert_eq!(RunProfile::from(ProfileArg::Trusted), RunProfile::Trusted);
        assert_eq!(RunProfile::from(ProfileArg::Platform), RunProfile::Platform);
        assert_eq!(RunProfile::from(ProfileArg::Confined), RunProfile::Confined);
    }

    #[test]
    fn explicit_platform_profile_is_honoured() {
        let cli = parse(&[
            "zeroship-migrate",
            "migrate",
            "--database-url",
            "x",
            "--profile",
            "platform",
        ]);
        assert_eq!(cli.profile, ProfileArg::Platform);
    }

    #[test]
    fn new_command_parses_name() {
        let cli = parse(&["zeroship-migrate", "new", "create_users"]);
        match cli.command {
            Command::New { name } => assert_eq!(name, "create_users"),
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[test]
    fn up_is_a_command() {
        let cli = parse(&["zeroship-migrate", "up", "--database-url", "x"]);
        assert!(matches!(cli.command, Command::Up));
    }

    #[test]
    fn down_is_a_command() {
        let cli = parse(&["zeroship-migrate", "down", "--database-url", "x"]);
        assert!(matches!(cli.command, Command::Down));
    }

    #[test]
    fn wait_parses_timeout_default_and_override() {
        let cli = parse(&["zeroship-migrate", "wait", "--database-url", "x"]);
        match cli.command {
            Command::Wait { timeout_secs } => assert_eq!(timeout_secs, 60),
            other => panic!("expected Wait, got {other:?}"),
        }
        let cli = parse(&[
            "zeroship-migrate",
            "wait",
            "--database-url",
            "x",
            "--timeout-secs",
            "5",
        ]);
        match cli.command {
            Command::Wait { timeout_secs } => assert_eq!(timeout_secs, 5),
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[test]
    fn dump_parses_schema_file_default_and_override() {
        let cli = parse(&["zeroship-migrate", "dump", "--database-url", "x"]);
        match cli.command {
            Command::Dump { schema_file } => {
                assert_eq!(schema_file, PathBuf::from(DEFAULT_SCHEMA_FILE));
            }
            other => panic!("expected Dump, got {other:?}"),
        }
        let cli = parse(&[
            "zeroship-migrate",
            "dump",
            "--database-url",
            "x",
            "--schema-file",
            "/tmp/s.sql",
        ]);
        match cli.command {
            Command::Dump { schema_file } => assert_eq!(schema_file, PathBuf::from("/tmp/s.sql")),
            other => panic!("expected Dump, got {other:?}"),
        }
    }

    #[test]
    fn lint_flag_parses_and_defaults_off() {
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        assert!(!cli.lint, "--lint defaults off");
        let cli = parse(&[
            "zeroship-migrate",
            "migrate",
            "--database-url",
            "x",
            "--lint",
        ]);
        assert!(cli.lint, "--lint opt-in");
    }

    #[test]
    fn dir_defaults_to_generic_db_migrations() {
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        assert_eq!(cli.dir, PathBuf::from(DEFAULT_DIR));
    }

    #[test]
    fn trusted_run_config_uses_generic_public_meta_and_single_schema() {
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        let cfg = run_config(&cli).expect("config builds");
        assert_eq!(cfg.profile, RunProfile::Trusted);
        assert_eq!(cfg.project_schema, DEFAULT_GENERIC_SCHEMA);
        assert_eq!(cfg.meta_schema, DEFAULT_GENERIC_SCHEMA);
        assert_eq!(cfg.schemas, vec![DEFAULT_GENERIC_SCHEMA.to_string()]);
        assert_eq!(cfg.project_id, "default");
    }

    #[test]
    fn platform_run_config_uses_zeroship_allowlist_and_migrations_meta() {
        let cli = parse(&[
            "zeroship-migrate",
            "migrate",
            "--database-url",
            "x",
            "--profile",
            "platform",
        ]);
        let cfg = run_config(&cli).expect("config builds");
        assert_eq!(cfg.profile, RunProfile::Platform);
        assert_eq!(cfg.project_schema, "zeroship");
        assert_eq!(cfg.meta_schema, "zeroship_migrations");
        assert!(cfg.schemas.contains(&"oauth_hydra".to_string()));
    }

    #[test]
    fn timestamp_14_is_14_digits_and_known_epoch() {
        // 2021-01-01T00:00:00Z = 1609459200 unix seconds.
        let t = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(timestamp_14(t), "20210101000000");
        // A second past gives a 14-digit stamp ending in 01.
        let t2 = UNIX_EPOCH + Duration::from_secs(1_609_459_201);
        assert_eq!(timestamp_14(t2), "20210101000001");
        // Sanity: always 14 digits.
        assert_eq!(timestamp_14(SystemTime::now()).len(), 14);
    }
}
