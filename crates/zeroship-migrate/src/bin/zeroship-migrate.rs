//! `zeroship-migrate` — the operator-side migration CLI (design §9, Phase 3).
//!
//! Replaces the Liquibase `migrate` compose service + `ops/db-migrate.sh`. It is
//! a THIN arg-parser: it parses flags with `clap`, builds a
//! [`RunConfig`](zeroship_migrate::guard::platform_runner::RunConfig)-equivalent,
//! and delegates to the `guard::platform_runner::run_*` functions — the ONLY place
//! the `OperatorCapability` token is minted (the §5 trust invariant). `main` never
//! touches the guard internals or mints a capability itself.
//!
//! compio, ZERO tokio: `#[compio::main]` drives the same compio-native `connect`
//! + executor the library uses.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

// The runner module is `pub(crate)`; the bin is part of the same crate so it can
// reach it. External crates cannot — the token mint stays confined.
use zeroship_migrate::guard::platform_runner::{
    self, default_platform_extensions, default_platform_schemas, RunConfig, RunProfile, RunReport,
};

/// Operator-side DB migration runner for the zeroship platform schemas.
#[derive(Debug, Parser)]
#[command(name = "zeroship-migrate", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Migration directory (Flyway-style `V<NNNN>__*.sql` / `.down.sql` / `R__`).
    #[arg(long, global = true, default_value = "db/migrations/")]
    dir: PathBuf,

    /// Admin Postgres DSN. Falls back to the `DATABASE_URL` env var.
    #[arg(long, global = true, env = "DATABASE_URL")]
    database_url: Option<String>,

    /// Trust profile. `platform` (DEFAULT for this binary — the ONLY place
    /// `platform` is selectable) widens the guard for the platform schemas;
    /// `confined` is the full creator deny-list, single-schema.
    #[arg(long, global = true, value_enum, default_value_t = ProfileArg::Platform)]
    profile: ProfileArg,

    /// The Platform schema allowlist (repeatable). Default:
    /// `zeroship`, `oauth_hydra`, `public`. The FIRST value is the primary schema
    /// pinned into `search_path`.
    #[arg(long = "schema", global = true)]
    schema: Vec<String>,

    /// The `CREATE EXTENSION` allowlist (repeatable). Default: `citext`,
    /// `uuid-ossp`.
    #[arg(long = "extension", global = true)]
    extension: Vec<String>,

    /// The meta schema holding the append-only journal. Default:
    /// `<primary-schema>_migrations` (e.g. `zeroship_migrations`).
    #[arg(long, global = true)]
    meta_schema: Option<String>,

    /// The advisory-lock serialization sentinel + journal project id. Default
    /// `platform` — two concurrent `migrate` runs hash to the same
    /// `pg_advisory_lock(hashtext(project_id))` and serialize (§9).
    #[arg(long, global = true, default_value = "platform")]
    project_id: String,

    /// Approve a DESTRUCTIVE plan (required by `migrate` when the plan is
    /// destructive, and ALWAYS by `rollback`). `--allow-destructive` is an alias.
    #[arg(long, global = true, visible_alias = "allow-destructive")]
    yes: bool,

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
    /// The widened platform guard (the binary default).
    Platform,
    /// The full creator deny-list, single-schema.
    Confined,
}

impl From<ProfileArg> for RunProfile {
    fn from(p: ProfileArg) -> Self {
        match p {
            ProfileArg::Platform => Self::Platform,
            ProfileArg::Confined => Self::Confined,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply all pending migrations (the compose `migrate` replacement). Requires
    /// `--yes`/`--allow-destructive` when the plan is destructive.
    Migrate,
    /// Print applied vs pending (+ rolled-back). Reads the journal; no DDL.
    Status,
    /// Dry-run on a shadow DB + report checksum drift + destructive advisories.
    /// NO DDL on the real DB.
    Validate,
    /// Roll back applied migrations via their `.down.sql` (gated; requires
    /// `--yes`). `--to <version>` unwinds everything after that numeric version;
    /// `--steps <N>` unwinds the N most-recent. Neither ⇒ roll back ALL.
    Rollback {
        /// Roll back everything strictly AFTER this numeric file version.
        #[arg(long, conflicts_with = "steps")]
        to: Option<u64>,
        /// Roll back the N most-recently-applied migrations.
        #[arg(long)]
        steps: Option<usize>,
    },
}

/// Build the runner [`RunConfig`] from the parsed CLI args (defaults applied here,
/// per §9). `main` stays thin: parse → this → delegate.
fn run_config(cli: &Cli) -> Result<RunConfig, String> {
    let database_url = cli
        .database_url
        .clone()
        .ok_or_else(|| "missing --database-url (or DATABASE_URL env)".to_string())?;

    let schemas = if cli.schema.is_empty() {
        default_platform_schemas()
    } else {
        cli.schema.clone()
    };
    let extensions = if cli.extension.is_empty() {
        default_platform_extensions()
    } else {
        cli.extension.clone()
    };
    // The primary schema (search_path target + Confined sole schema) is the first
    // allowlist entry.
    let project_schema = schemas
        .first()
        .cloned()
        .ok_or_else(|| "schema allowlist is empty".to_string())?;
    let meta_schema = cli
        .meta_schema
        .clone()
        .unwrap_or_else(|| format!("{project_schema}_migrations"));

    Ok(RunConfig {
        dir: cli.dir.clone(),
        database_url,
        profile: cli.profile.into(),
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

#[compio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let cfg = match run_config(&cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zeroship-migrate: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = match &cli.command {
        Command::Migrate => platform_runner::run_migrate(&cfg).await,
        Command::Status => platform_runner::run_status(&cfg).await,
        Command::Validate => platform_runner::run_validate(&cfg).await,
        Command::Rollback { to, steps } => {
            platform_runner::run_rollback(&cfg, *to, *steps).await
        }
    };

    match result {
        Ok(report) => {
            print_report(&report);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zeroship-migrate: {e}");
            ExitCode::FAILURE
        }
    }
}
