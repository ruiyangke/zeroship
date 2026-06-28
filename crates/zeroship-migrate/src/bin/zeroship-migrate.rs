//! `zeroship-migrate` — the migration CLI (design §9, Phase 3; Track A, Phase A3).
//!
//! A dbmate-like, generic-by-default migration tool. `main` is a THIN arg-parser:
//! it parses flags with `clap`, builds a
//! [`RunConfig`](zeroship_migrate::command::runner::RunConfig)-equivalent,
//! and delegates to the `command::runner::run_*` functions — the ONLY place
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
use zeroship_migrate::command::runner::{
    self as command_runner, default_platform_extensions, default_platform_schemas, Engine,
    EngineKind, RunConfig, RunError, RunProfile, RunReport,
};
use zeroship_migrate::plan::loader::{
    is_valid_migration_name, new_dbmate_migration, suggest_migration_name,
};

mod config;
use config::FileEnvLayer;

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
    /// auto-detected. Precedence (highest first): this flag, then the
    /// `ZEROSHIP_MIGRATE_MIGRATIONS_DIR` env, then `migrations_dir` in
    /// `zeroship-migrate.toml`, then the default `./db/migrations`. No clap
    /// `default_value`, so the bin can tell "flag absent" from "flag given".
    #[arg(long, global = true)]
    dir: Option<PathBuf>,

    /// Postgres/SQLite DSN. Precedence (highest first): this flag, then a NON-EMPTY
    /// `DATABASE_URL` env, then `database_url` in `zeroship-migrate.toml`. We do NOT
    /// use clap's `env=` here: clap 4 yields `Some("")` for a present-but-empty var,
    /// which would defeat the config fallback (and `classify_engine("")` is an
    /// Unsupported error). Instead the bin reads `DATABASE_URL` explicitly through the
    /// SAME empty-is-unset rule the four `ZEROSHIP_MIGRATE_*` vars use, so an empty
    /// value falls through to the config DSN (MED-1). This field holds ONLY the
    /// `--database-url` flag value.
    #[arg(long, global = true)]
    database_url: Option<String>,

    /// Force the database engine, overriding DSN-shape auto-detection: `pg` |
    /// `sqlite` | `mysql`. Resolves an ambiguous bare-path DSN; ERRORS if it contradicts an
    /// unambiguous DSN scheme (e.g. `--engine sqlite` with a `postgres://` URL).
    /// Precedence (highest first): this flag, then `ZEROSHIP_MIGRATE_ENGINE`, then
    /// `engine` in the config. Absent means DSN auto-detect (no behaviour change for
    /// existing runs).
    #[arg(long, global = true, value_enum)]
    engine: Option<EngineArg>,

    /// Trust profile. `trusted` (DEFAULT — the public posture: the operator owns
    /// the DB, so the deny-list is OFF and there is no schema confinement);
    /// `platform` widens the guard for the zeroship platform schemas (the
    /// internal compose/ops posture — must be passed EXPLICITLY); `confined` is
    /// the full creator deny-list, single-schema. Precedence: this flag >
    /// `ZEROSHIP_MIGRATE_PROFILE` env > `profile` in the config > default `trusted`.
    #[arg(long, global = true, value_enum)]
    profile: Option<ProfileArg>,

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

    /// Schema-dump file path — the output of `dump`, the input of `load`, and the
    /// target of the auto-refresh after `migrate`/`up`/`rollback`/`down`. Global so
    /// it is available on every command (dbmate's `--schema-file`). Precedence
    /// (highest first): this flag, then `ZEROSHIP_MIGRATE_SCHEMA_FILE` env, then
    /// `schema_file` in `zeroship-migrate.toml`, then the default `./db/schema.sql`.
    #[arg(long, global = true)]
    schema_file: Option<PathBuf>,

    /// Suppress the automatic `schema.sql` refresh that follows a successful
    /// `migrate`/`up`/`rollback`/`down` (dbmate's `--no-dump-schema`). Auto-dump is
    /// ON by default (dbmate parity). Precedence (highest first): this flag, then
    /// `ZEROSHIP_MIGRATE_DUMP_SCHEMA` env, then `dump_schema` in the config; default
    /// = auto-dump ON. The flag is one-directional (it can only turn auto-dump OFF);
    /// to force it ON despite a config that disables it, simply omit the flag.
    #[arg(long, global = true)]
    no_dump_schema: bool,
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

impl ProfileArg {
    /// Parse a profile from an env-var / config-file string (case-insensitive). The
    /// SAME accepted set as the clap `--profile` value-enum, so the three layers
    /// agree.
    fn from_config_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trusted" => Ok(Self::Trusted),
            "platform" => Ok(Self::Platform),
            "confined" => Ok(Self::Confined),
            other => Err(format!(
                "invalid profile {other:?} (allowed: trusted, platform, confined)"
            )),
        }
    }
}

/// The CLI engine-override flag value (`--engine pg|sqlite|mysql`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum EngineArg {
    /// Force the Postgres backend.
    Pg,
    /// Force the `SQLite` backend.
    Sqlite,
    /// Force the MySQL render-only dialect.
    Mysql,
}

impl From<EngineArg> for EngineKind {
    fn from(e: EngineArg) -> Self {
        match e {
            EngineArg::Pg => Self::Postgres,
            EngineArg::Sqlite => Self::Sqlite,
            EngineArg::Mysql => Self::Mysql,
        }
    }
}

impl EngineArg {
    /// Parse an engine from an env-var / config-file string (case-insensitive).
    /// `postgres` is accepted as a friendly alias of `pg`. SAME accepted set the
    /// clap value-enum exposes (plus the alias), so the layers agree.
    fn from_config_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pg" | "postgres" | "postgresql" => Ok(Self::Pg),
            "sqlite" => Ok(Self::Sqlite),
            "mysql" | "mysql8" | "mariadb" => Ok(Self::Mysql),
            other => Err(format!("invalid engine {other:?} (allowed: pg, sqlite, mysql)")),
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a new dbmate-format raw-`.sql` migration file
    /// (`<14-digit-ts>_<name>.sql`) in `--dir`, with empty `-- migrate:up`/`down`
    /// sections. Prints the created path.
    ///
    /// NOTE (PR7 raw-SQL demotion): this raw-`.sql` authoring path is **RETAINED for
    /// the dbmate-style operator CLI and the platform Flyway-mode** — it is NOT the
    /// recommended creator authoring path. Creators author portable, bi-dialect
    /// (PG + SQLite) migrations as op.* `@zeroship/migrate` `.ts` modules (compiled to
    /// `.ir.json`) via `zeroship-migrate-js new`/`generate` — the DSL expresses the
    /// full DDL + DML + online surface and never drops to raw SQL (`docs/proposals/
    /// 2026-06-23-js-op-dsl-migration-design-normative.md` §PR7). Use this raw-`.sql`
    /// `new` only for the operator/platform path.
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
    /// Render the exact per-dialect SQL the pending migration set WOULD execute,
    /// WITHOUT a DB and WITHOUT applying (the canonical Alembic `--sql` / Flyway /
    /// dbmate feature). For OPERATOR GO-LIVE REVIEW — review the exact SQL before
    /// approving an `approved_versions` go-live (vs approving blind).
    ///
    /// DB-state-dependent ops (online-rename backfill/cutover, SQLite rebuild,
    /// existence-guarded ops) print a labeled `-- [runtime-resolved]` line, never
    /// fabricated SQL. DISTINCT from `validate` (the DB-backed shadow dry-run that
    /// APPLIES on a throwaway). Loads `.sql` (Flyway/dbmate) and/or `.ir.json`
    /// (creator) artifacts from `--dir`; opens NO DB connection.
    Plan {
        /// Render for this dialect (`pg` | `sqlite` | `mysql`). Default: the `--engine`
        /// override if present, else `pg`. NO DB connection is opened to pick it.
        #[arg(long, value_enum)]
        dialect: Option<EngineArg>,
    },
    /// Run the advisory analyzers over the migration artifacts in `--dir`, without
    /// opening a DB connection. Default exit is 0 (advisories are informational).
    /// `--deny-warnings` exits non-zero on Warning-severity advisories; `--deny
    /// RULE[,RULE...]` exits non-zero on those stable analyzer rule ids.
    Lint {
        /// Analyze rendered creator `.ir.json` statements for this dialect (`pg` |
        /// `sqlite` | `mysql`). Default: the `--engine` override if present, else
        /// `pg`. Raw `.sql` artifacts are analyzed verbatim, same as `plan`.
        #[arg(long, value_enum)]
        dialect: Option<EngineArg>,
        /// Emit a machine-readable array of
        /// `{ migration, rule, severity, message, suggestion }`.
        #[arg(long)]
        json: bool,
        /// Exit non-zero when any Warning-severity advisory is present. Notices do
        /// not fail this gate.
        #[arg(long)]
        deny_warnings: bool,
        /// Comma-separated stable analyzer rule ids that should make lint fail
        /// when present, regardless of severity.
        #[arg(long, value_name = "RULE[,RULE...]")]
        deny: Option<String>,
    },
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
    Dump,
    /// Bootstrap a FRESH database to the current schema FAST by REPLAYING the dumped
    /// `schema.sql` (dbmate `db:load` / `db:setup`), then reconstructing the journal
    /// from the dump's applied-versions trailer — so `status` reports those versions
    /// applied and a subsequent `migrate` runs only the genuinely-pending ones. Both
    /// engines. Refuses (non-zero) if the schema file is missing/unreadable or the
    /// target DB is already engine-managed (first-entry-only — never clobbers). The
    /// schema file is the global `--schema-file` (default `./db/schema.sql`).
    #[command(visible_alias = "setup")]
    Load,
    /// Resolve a cross-deploy online-rename PENDING CONTRACT (§2.0.3). After an
    /// online `renameColumn` EXPAND applies, its contract (drop dual-write trigger
    /// and drop old column) is deferred to a later deploy and held as a durable
    /// obligation that fail-closed refuses further changes to the table. This
    /// command discharges it. GATED on `--yes` (both paths are destructive). PG
    /// only — a SQLite rename is atomic and has no pending partition.
    ResolvePending {
        /// Apply the deferred contract (drop trigger + drop old column) — the
        /// rename completes. Destructive (C2). Mutually exclusive with `--abort`.
        #[arg(long, conflicts_with = "abort")]
        apply: bool,
        /// Abort the rename: drop the shadow column + dual-write trigger, returning
        /// the table to its pre-rename shape. Destructive. Mutually exclusive with
        /// `--apply`.
        #[arg(long)]
        abort: bool,
        /// **REQUIRED for `--abort`** (and ONLY `--abort`): acknowledge that aborting
        /// DISCARDS data written only to the shadow `to` column since cutover (the
        /// shadow column is dropped). This is a DISTINCT acknowledgement, separate from
        /// the shared `--yes` that `--apply` uses — a blanket `--yes` must not silently
        /// authorize the data-discarding abort. `--apply` ignores this flag (it
        /// preserves data).
        #[arg(long)]
        acknowledge_shadow_data_loss: bool,
        /// The pending contract's version (the EXPAND/`pending_version` reported by
        /// `status`).
        version: String,
    },
}

/// The effective migration directory under the precedence rule (CLI flag > env >
/// config file > default `./db/migrations`). The clap `--dir` flag, when present,
/// is highest; otherwise the folded file+env `migrations_dir` (env already wins
/// over file inside [`FileEnvLayer`]); otherwise the built-in default.
fn effective_dir(cli: &Cli, layer: &FileEnvLayer) -> PathBuf {
    cli.dir
        .clone()
        .or_else(|| layer.migrations_dir.clone().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR))
}

/// The effective schema-dump path (CLI `--schema-file` > env > config > default).
/// `flag` is the `Dump { schema_file }` value (already highest).
fn effective_schema_file(flag: Option<&Path>, layer: &FileEnvLayer) -> PathBuf {
    flag.map(Path::to_path_buf)
        .or_else(|| layer.schema_file.clone().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SCHEMA_FILE))
}

/// Whether to auto-refresh `schema.sql` after a successful schema-changing command
/// (dbmate parity). Precedence: the CLI `--no-dump-schema` flag (one-directional —
/// it can only turn auto-dump OFF) > the folded `dump_schema` env/config value >
/// the built-in default ON. `--no-dump-schema` always wins (it is the explicit
/// operator opt-out for this invocation).
fn effective_auto_dump(cli: &Cli, layer: &FileEnvLayer) -> bool {
    if cli.no_dump_schema {
        return false;
    }
    layer.dump_schema.unwrap_or(true)
}

/// The effective trust profile (CLI flag > env > config > default `trusted`).
fn effective_profile(cli: &Cli, layer: &FileEnvLayer) -> Result<RunProfile, String> {
    if let Some(p) = cli.profile {
        return Ok(p.into());
    }
    if let Some(s) = &layer.profile {
        return Ok(ProfileArg::from_config_str(s)?.into());
    }
    Ok(RunProfile::Trusted)
}

/// The effective engine override (CLI flag > env > config). `None` ⇒ DSN
/// auto-detect (the default; no behaviour change for existing invocations).
fn effective_engine(cli: &Cli, layer: &FileEnvLayer) -> Result<Option<EngineKind>, String> {
    if let Some(e) = cli.engine {
        return Ok(Some(e.into()));
    }
    if let Some(s) = &layer.engine {
        return Ok(Some(EngineArg::from_config_str(s)?.into()));
    }
    Ok(None)
}

/// Build the runner [`RunConfig`] from the parsed CLI args + the folded file+env
/// layer (precedence applied here, per §9). `main` stays thin: parse → this →
/// delegate.
fn run_config(cli: &Cli, layer: &FileEnvLayer) -> Result<RunConfig, String> {
    // DSN precedence: --database-url flag > non-empty DATABASE_URL env > config
    // `database_url`. The env (empty-is-unset) and file are already folded into
    // `layer.database_url` by `FileEnvLayer::resolve`; the flag wins on top here.
    let database_url = cli
        .database_url
        .clone()
        .or_else(|| layer.database_url.clone())
        .ok_or_else(|| {
            "missing database URL (pass --database-url, set DATABASE_URL, or set \
             `database_url` in zeroship-migrate.toml)"
                .to_string()
        })?;

    let engine_override = effective_engine(cli, layer)?;
    let profile: RunProfile = effective_profile(cli, layer)?;

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
        dir: effective_dir(cli, layer),
        database_url,
        engine_override,
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

#[derive(Debug, serde::Serialize)]
struct LintFinding {
    migration: String,
    rule: String,
    severity: zeroship_migrate::Severity,
    message: String,
    suggestion: Option<String>,
}

/// Print advisory lint in the standalone command's human format. The legacy
/// post-`migrate --lint` side-effect reuses this formatter; its exit semantics stay
/// unchanged in `main`.
fn print_lint(pairs: &[(String, Vec<zeroship_migrate::Advisory>)]) {
    let (warnings, notices) = lint_counts(pairs);
    if warnings == 0 && notices == 0 {
        println!("lint: no advisories");
    } else {
        println!("lint:");
        for (migration, advisories) in pairs {
            if advisories.is_empty() {
                continue;
            }
            println!("  {migration}:");
            for severity in [
                zeroship_migrate::Severity::Warning,
                zeroship_migrate::Severity::Notice,
            ] {
                for advisory in advisories.iter().filter(|a| a.severity == severity) {
                    println!(
                        "    {} {}  {}",
                        severity_label(advisory.severity),
                        advisory.rule,
                        advisory.message
                    );
                    if let Some(suggestion) = &advisory.suggestion {
                        println!("      suggestion: {suggestion}");
                    }
                }
            }
        }
    }
    println!(
        "summary: {warnings} {}, {notices} {}",
        plural(warnings, "warning"),
        plural(notices, "notice")
    );
}

fn print_lint_json(pairs: &[(String, Vec<zeroship_migrate::Advisory>)]) {
    let findings = lint_findings(pairs);
    println!(
        "{}",
        serde_json::to_string_pretty(&findings).expect("lint findings serialize")
    );
}

fn lint_findings(pairs: &[(String, Vec<zeroship_migrate::Advisory>)]) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    for (migration, advisories) in pairs {
        for advisory in advisories {
            findings.push(LintFinding {
                migration: migration.clone(),
                rule: advisory.rule.to_string(),
                severity: advisory.severity,
                message: advisory.message.clone(),
                suggestion: advisory.suggestion.clone(),
            });
        }
    }
    findings
}

fn lint_counts(pairs: &[(String, Vec<zeroship_migrate::Advisory>)]) -> (usize, usize) {
    let warnings = pairs
        .iter()
        .flat_map(|(_, advisories)| advisories)
        .filter(|a| a.severity == zeroship_migrate::Severity::Warning)
        .count();
    let notices = pairs
        .iter()
        .flat_map(|(_, advisories)| advisories)
        .filter(|a| a.severity == zeroship_migrate::Severity::Notice)
        .count();
    (warnings, notices)
}

const fn severity_label(severity: zeroship_migrate::Severity) -> &'static str {
    match severity {
        zeroship_migrate::Severity::Warning => "Warning",
        zeroship_migrate::Severity::Notice => "Notice",
    }
}

fn plural(count: usize, singular: &str) -> &'static str {
    match singular {
        "warning" if count == 1 => "warning",
        "warning" => "warnings",
        "notice" if count == 1 => "notice",
        "notice" => "notices",
        _ => "items",
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
        RunReport::Load(outcome) => {
            println!(
                "load: restored schema, journaled {} version(s) {:?}",
                outcome.versions.len(),
                outcome.versions
            );
        }
        RunReport::ResolvePending(outcome) => {
            println!(
                "resolve-pending: {} contract for table `{}` (version {}); applied {:?}",
                outcome.resolution.as_str(),
                outcome.table,
                outcome.pending_version,
                outcome.applied
            );
            // **PR9b L3** — surface the abort's data-loss warning PROMINENTLY (to
            // stderr) so it is never lost in the routine line above.
            if let Some(warning) = &outcome.data_loss_warning {
                eprintln!("WARNING: {warning}");
            }
        }
    }
}

/// `new` — generate a dbmate-format migration file in the effective `dir`. VALIDATES
/// the name against the loader's accepted grammar BEFORE writing (so a name the
/// loader would later reject as `UnrecognizedFile` is refused at scaffold time, with
/// NO file written), errors if the file already exists (never clobbers), and prints
/// the created path. The name is NEVER auto-renamed — an invalid name is rejected
/// with a normalized suggestion.
fn run_new(dir: &Path, name: &str) -> Result<(), String> {
    // Validate FIRST — before any I/O — using the exact grammar `load_dir` enforces.
    if !is_valid_migration_name(name) {
        let suggestion = suggest_migration_name(name);
        let hint = if suggestion.is_empty() {
            String::new()
        } else {
            format!(" (did you mean `{suggestion}`?)")
        };
        return Err(format!(
            "invalid migration name {name:?}: allowed characters are [A-Za-z0-9_] \
             (use `_` for spaces/dashes){hint}"
        ));
    }
    let stamp = timestamp_14(SystemTime::now());
    let (filename, contents) = new_dbmate_migration(&stamp, name);
    let path = dir.join(&filename);
    if path.exists() {
        return Err(format!("refusing to overwrite existing file: {}", path.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("create dir {}: {e}", dir.display()))?;
    std::fs::write(&path, contents).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!("Creating migration: {}", path.display());
    Ok(())
}

#[derive(Debug)]
struct OfflineArtifacts {
    ir_files: Vec<PathBuf>,
    has_sql: bool,
}

const fn preview_dialect(chosen: Option<EngineArg>) -> zeroship_schema::query::SqlDialect {
    match chosen {
        Some(EngineArg::Sqlite) => zeroship_schema::query::SqlDialect::Sqlite,
        Some(EngineArg::Mysql) => zeroship_schema::query::SqlDialect::Mysql,
        // `--dialect pg`, `--engine pg`, or unset all render/analyze the PG leg
        // (the default operator target). NO DSN probe.
        _ => zeroship_schema::query::SqlDialect::Postgres,
    }
}

fn preview_opts() -> zeroship_migrate::PreviewOpts {
    zeroship_migrate::PreviewOpts {
        default_schema: DEFAULT_GENERIC_SCHEMA.to_string(),
        owner_app: "app_preview".to_string(),
    }
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn discover_offline_artifacts(dir: &Path, command: &str) -> Result<OfflineArtifacts, String> {
    let read = std::fs::read_dir(dir)
        .map_err(|e| format!("{command}: read dir {}: {e}", dir.display()))?;
    let mut ir_files: Vec<PathBuf> = Vec::new();
    let mut has_sql = false;
    for entry in read {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let lower_name = name.to_ascii_lowercase();
        if lower_name.ends_with(".ir.json") {
            ir_files.push(path);
        } else if lower_name.ends_with(".sql") && !lower_name.ends_with(".down.sql") {
            has_sql = true;
        }
    }
    ir_files.sort();

    if !has_sql && ir_files.is_empty() {
        return Err(format!(
            "{command}: no `.sql` or `.ir.json` artifacts found in {}",
            dir.display()
        ));
    }

    Ok(OfflineArtifacts { ir_files, has_sql })
}

/// `plan --sql` — the OFFLINE per-dialect SQL plan preview (PR14). Loads `.sql`
/// (Flyway/dbmate) and/or `.ir.json` (creator) artifacts from `dir`, renders the
/// exact SQL the engine WOULD run via the pure, DB-free
/// [`zeroship_migrate::render::sql_preview`] surfacing layer, and prints it to stdout. DB-
/// state-dependent ops print `-- [runtime-resolved]` labels, never fabricated SQL.
///
/// Opens NO DB connection (a unit/integration test asserts this offline path). The
/// dialect is `chosen` (`--dialect` > `--engine`), defaulting to Postgres — NEVER
/// probed from a live DSN. Returns the process exit code: SUCCESS on a rendered
/// preview, FAILURE on a missing/unreadable dir or an unloadable artifact.
fn run_plan_preview(
    dir: &Path,
    chosen: Option<EngineArg>,
    _layer: &FileEnvLayer,
) -> ExitCode {
    use zeroship_migrate::render::sql_preview::{render_ir_json_sql, render_set_sql};

    let dialect = preview_dialect(chosen);
    let opts = preview_opts();
    let artifacts = match discover_offline_artifacts(dir, "plan") {
        Ok(a) => a,
        Err(e) => {
            eprintln!("zeroship-migrate: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut output = String::new();

    // The `.sql` (Flyway/dbmate) leg: `load_dir` lowers each to a single-step plan
    // (no DB), then the set renderer formats them.
    if artifacts.has_sql {
        match zeroship_migrate::plan::loader::load_dir(dir) {
            Ok(plans) => {
                output.push_str(&render_set_sql(&plans, dialect, &opts));
            }
            Err(e) => {
                eprintln!("zeroship-migrate: plan: load `.sql` artifacts: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // The `.ir.json` (creator) leg: load + lower each offline, per dialect.
    for path in &artifacts.ir_files {
        let bytes = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("zeroship-migrate: plan: read {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        };
        match render_ir_json_sql(&bytes, dialect, &opts) {
            Ok(text) => {
                output.push('\n');
                output.push_str(&text);
            }
            Err(e) => {
                eprintln!("zeroship-migrate: plan: {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }

    print!("{output}");
    ExitCode::SUCCESS
}

fn run_lint_command(
    dir: &Path,
    chosen: Option<EngineArg>,
    json: bool,
    deny_warnings: bool,
    deny: Option<&str>,
) -> ExitCode {
    let dialect = preview_dialect(chosen);
    let opts = preview_opts();
    let pairs = match lint_offline_artifacts(dir, dialect, &opts) {
        Ok(pairs) => pairs,
        Err(e) => {
            eprintln!("zeroship-migrate: lint: {e}");
            return ExitCode::FAILURE;
        }
    };

    if json {
        print_lint_json(&pairs);
    } else {
        print_lint(&pairs);
    }

    if lint_denied(&pairs, deny_warnings, deny) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn lint_offline_artifacts(
    dir: &Path,
    dialect: zeroship_schema::query::SqlDialect,
    opts: &zeroship_migrate::PreviewOpts,
) -> Result<Vec<(String, Vec<zeroship_migrate::Advisory>)>, String> {
    let artifacts = discover_offline_artifacts(dir, "lint")?;
    let mut pairs = Vec::new();

    if artifacts.has_sql {
        let plans = zeroship_migrate::plan::loader::load_dir(dir)
            .map_err(|e| format!("load `.sql` artifacts: {e}"))?;
        for plan in &plans {
            let migration = plan
                .single_step_migration()
                .map_err(|e| format!("load `.sql` artifacts: {e}"))?;
            pairs.push((
                migration.version.as_str().to_string(),
                zeroship_migrate::analyze_migration(migration),
            ));
        }
    }

    for path in &artifacts.ir_files {
        let bytes = std::fs::read_to_string(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let (name, statements) =
            zeroship_migrate::render_ir_json_sql_statements(&bytes, dialect, opts)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        let migration = if name.is_empty() {
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("<ir>")
                .to_string()
        } else {
            name
        };
        let mut advisories = Vec::new();
        for statement in statements {
            advisories.extend(analyze_rendered_statement(&statement));
        }
        pairs.push((migration, advisories));
    }

    Ok(pairs)
}

fn analyze_rendered_statement(statement: &str) -> Vec<zeroship_migrate::Advisory> {
    zeroship_migrate::RawSqlAuthor::new(DEFAULT_GENERIC_SCHEMA, "app_preview")
        .wrap("lint_rendered_statement", statement, None)
        .map_or_else(
            |_| zeroship_migrate::analyze(statement),
            |migration| zeroship_migrate::analyze_migration(&migration),
        )
}

fn lint_denied(
    pairs: &[(String, Vec<zeroship_migrate::Advisory>)],
    deny_warnings: bool,
    deny: Option<&str>,
) -> bool {
    let deny_rules = deny
        .map(|rules| {
            rules
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_ascii_uppercase)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();

    pairs.iter().flat_map(|(_, advisories)| advisories).any(|a| {
        (deny_warnings && a.severity == zeroship_migrate::Severity::Warning)
            || deny_rules.contains(&a.rule.to_ascii_uppercase())
    })
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
    // The schema body is engine-specific; the trailer is shared (below). The engine
    // honours `--engine` via `resolve_engine` (a conflict is an honest error).
    let mut schema = match cfg.resolve_engine().map_err(|e| e.to_string())? {
        Engine::Postgres => dump_schema_pg(&cfg.database_url)?,
        Engine::Sqlite(app) => command_runner::dump_schema_sqlite(&app)
            .await
            .map_err(|e| e.to_string())?,
        Engine::Mysql => return Err(RunError::MysqlLiveExecUnimplemented.to_string()),
        Engine::Unsupported => return Err(RunError::UnsupportedEngine.to_string()),
    };

    // Append the applied-versions trailer (dbmate writes the schema_migrations
    // rows so a fresh `db:setup` knows which versions are already applied). Each
    // entry carries the version + checksum + name sourced from the JOURNAL so the
    // trailer is SELF-CONTAINED: `load` reconstructs the journal from the trailer
    // alone (M1+M2), never re-deriving checksum/name from `--dir`. Tab-delimited so
    // a name with spaces round-trips. Identical shape for BOTH engines.
    let entries = command_runner::run_dump_trailer(cfg)
        .await
        .map_err(|e| format!("read journal for dump trailer: {e}"))?;
    schema.push_str("\n-- zeroship-migrate schema_migrations\n");
    for e in &entries {
        let _ = writeln!(schema, "--   {}\t{}\t{}", e.version, e.checksum, e.name);
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

#[compio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Discover + fold the project config file (CWD, walking up) with the process
    // env into the file+env precedence layer. A MISSING file is fine; a MALFORMED
    // one is a hard, clear error (never silently ignored).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let discovered = match config::discover_file_config(&cwd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zeroship-migrate: {e}");
            return ExitCode::FAILURE;
        }
    };
    // LOW-3: when a config IS discovered+loaded, echo its path to STDERR so an
    // ancestor-dir `zeroship-migrate.toml` (possibly carrying a `database_url`) is
    // never adopted invisibly. Silent when none is found. Canonicalize for an
    // absolute path; fall back to the discovered (relative) path if that fails.
    if let Some(d) = &discovered {
        let shown = std::fs::canonicalize(&d.path).unwrap_or_else(|_| d.path.clone());
        eprintln!("using config: {}", shown.display());
    }
    let file_cfg = discovered.map(|d| d.config);
    let layer = FileEnvLayer::resolve(file_cfg.as_ref());

    // `new` is offline (no DB): handle it before building a DSN-bearing RunConfig.
    if let Command::New { name } = &cli.command {
        let dir = effective_dir(&cli, &layer);
        return match run_new(&dir, name) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("zeroship-migrate: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // `plan --sql` is OFFLINE (no DB): render the exact per-dialect SQL the pending
    // set WOULD run, for operator go-live review. Dispatch BEFORE building a
    // DSN-bearing RunConfig — it opens no connection, mints no capability. The
    // dialect comes from `--dialect`, else the `--engine` override, else `pg` (NEVER
    // from a live DSN probe). It loads `.sql` (Flyway/dbmate) AND `.ir.json`
    // (creator) artifacts from `--dir`.
    if let Command::Plan { dialect } = &cli.command {
        let dir = effective_dir(&cli, &layer);
        let chosen = dialect.or(cli.engine);
        return run_plan_preview(&dir, chosen, &layer);
    }

    // `lint` is OFFLINE (no DB): run the advisory analyzers over the same artifact
    // set `plan` previews. Dispatch BEFORE building a DSN-bearing RunConfig.
    if let Command::Lint { dialect, json, deny_warnings, deny } = &cli.command {
        let dir = effective_dir(&cli, &layer);
        let chosen = dialect.or(cli.engine);
        return run_lint_command(&dir, chosen, *json, *deny_warnings, deny.as_deref());
    }

    let cfg = match run_config(&cli, &layer) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zeroship-migrate: {e}");
            return ExitCode::FAILURE;
        }
    };

    // `wait` polls the DSN; no migration load. Handle it directly.
    if let Command::Wait { timeout_secs } = &cli.command {
        let res = command_runner::run_wait(
            &cfg.database_url,
            cfg.engine_override,
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
    if let Command::Dump = &cli.command {
        let out = effective_schema_file(cli.schema_file.as_deref(), &layer);
        return match run_dump(&cfg, &out).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("zeroship-migrate: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // `load` (db:setup) replays a dumped schema.sql + reconstructs the journal; no
    // migration plan. Read the schema file FIRST (a missing/unreadable file is a
    // clean non-zero refusal BEFORE touching the DB), then dispatch by engine.
    if let Command::Load = &cli.command {
        let path = effective_schema_file(cli.schema_file.as_deref(), &layer);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "zeroship-migrate: load: read schema file {}: {e}",
                    path.display()
                );
                return ExitCode::FAILURE;
            }
        };
        return match command_runner::run_load(&cfg, &content).await {
            Ok(report) => {
                println!("load: replayed {}", path.display());
                print_report(&report);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("zeroship-migrate: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let result = match &cli.command {
        Command::Migrate | Command::Up => command_runner::run_migrate(&cfg).await,
        Command::Down => command_runner::run_down(&cfg).await,
        Command::Status => command_runner::run_status(&cfg).await,
        Command::Validate => command_runner::run_validate(&cfg).await,
        Command::Rollback { to, steps } => {
            command_runner::run_rollback(&cfg, *to, *steps).await
        }
        Command::ResolvePending { apply, abort, acknowledge_shadow_data_loss, version } => {
            command_runner::run_resolve_pending(
                &cfg,
                version,
                *apply,
                *abort,
                *acknowledge_shadow_data_loss,
            )
            .await
        }
        // Handled above (offline / non-plan commands).
        Command::New { .. }
        | Command::Wait { .. }
        | Command::Dump
        | Command::Load
        | Command::Plan { .. }
        | Command::Lint { .. } => {
            unreachable!()
        }
    };

    match result {
        Ok(report) => {
            print_report(&report);
            // `--lint`: opt-in, non-blocking advisories for migrate/up/validate.
            if cli.lint && matches!(cli.command, Command::Migrate | Command::Up | Command::Validate)
            {
                match command_runner::lint_advisories(&cfg) {
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
            // Auto-refresh `schema.sql` after a SUCCESSFUL schema-changing command
            // (dbmate parity): migrate/up/rollback/down. NEVER after read-only
            // status/validate (their reports are not Migrate/Rollback). A dump
            // failure here is NON-FATAL — the migration already committed; we warn
            // to stderr and keep the success exit. `--no-dump-schema` / config opt
            // out. The schema path is the global `--schema-file` (env/config/default).
            if matches!(report, RunReport::Migrate(_) | RunReport::Rollback(_))
                && effective_auto_dump(&cli, &layer)
            {
                let out = effective_schema_file(cli.schema_file.as_deref(), &layer);
                if let Err(e) = run_dump(&cfg, &out).await {
                    eprintln!(
                        "zeroship-migrate: warning: auto-dump of {} after a successful \
                         {:?} failed (the migration already committed): {e}",
                        out.display(),
                        cli.command
                    );
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

    /// An empty file+env layer (the no-config baseline): every precedence test that
    /// is not specifically about env/file uses this so the built-in defaults apply.
    fn empty_layer() -> FileEnvLayer {
        FileEnvLayer::default()
    }

    #[test]
    fn profile_defaults_to_trusted() {
        // No --profile flag, no env/config ⇒ the effective profile is Trusted.
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        assert_eq!(cli.profile, None, "flag is absent");
        assert_eq!(
            effective_profile(&cli, &empty_layer()).expect("profile"),
            RunProfile::Trusted
        );
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
        assert_eq!(cli.profile, Some(ProfileArg::Platform));
        assert_eq!(
            effective_profile(&cli, &empty_layer()).expect("profile"),
            RunProfile::Platform
        );
    }

    #[test]
    fn profile_precedence_flag_over_env_over_config() {
        let cli_flag = parse(&[
            "zeroship-migrate",
            "migrate",
            "--database-url",
            "x",
            "--profile",
            "confined",
        ]);
        // Flag beats a config that says platform.
        let layer = FileEnvLayer {
            profile: Some("platform".to_string()),
            ..FileEnvLayer::default()
        };
        assert_eq!(
            effective_profile(&cli_flag, &layer).expect("profile"),
            RunProfile::Confined,
            "flag wins"
        );
        // No flag ⇒ the folded layer's profile (env-or-config) wins over the default.
        let cli_noflag = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        assert_eq!(
            effective_profile(&cli_noflag, &layer).expect("profile"),
            RunProfile::Platform,
            "layer wins over the trusted default"
        );
    }

    #[test]
    fn engine_precedence_flag_over_layer_and_invalid_config_errors() {
        // No flag, no layer ⇒ None (DSN auto-detect).
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        assert_eq!(effective_engine(&cli, &empty_layer()).expect("engine"), None);
        // Flag wins over a contradicting layer value.
        let cli_flag = parse(&[
            "zeroship-migrate",
            "migrate",
            "--database-url",
            "x",
            "--engine",
            "pg",
        ]);
        let layer = FileEnvLayer {
            engine: Some("sqlite".to_string()),
            ..FileEnvLayer::default()
        };
        assert_eq!(
            effective_engine(&cli_flag, &layer).expect("engine"),
            Some(EngineKind::Postgres)
        );
        // No flag ⇒ the layer's `sqlite` resolves; `postgres` alias is accepted.
        assert_eq!(
            effective_engine(&cli, &layer).expect("engine"),
            Some(EngineKind::Sqlite)
        );
        let mysql_layer = FileEnvLayer {
            engine: Some("mysql".to_string()),
            ..FileEnvLayer::default()
        };
        assert_eq!(
            effective_engine(&cli, &mysql_layer).expect("engine"),
            Some(EngineKind::Mysql)
        );
        // An invalid layer engine is a clear error (not a silent ignore).
        let bad = FileEnvLayer {
            engine: Some("oracle".to_string()),
            ..FileEnvLayer::default()
        };
        assert!(effective_engine(&cli, &bad).is_err());
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
        // No flag ⇒ the global schema_file is None; the effective path is the default.
        let cli = parse(&["zeroship-migrate", "dump", "--database-url", "x"]);
        assert!(matches!(cli.command, Command::Dump));
        assert_eq!(cli.schema_file.as_deref(), None);
        assert_eq!(
            effective_schema_file(cli.schema_file.as_deref(), &empty_layer()),
            PathBuf::from(DEFAULT_SCHEMA_FILE)
        );
        // `--schema-file` is now a GLOBAL flag (dbmate parity); it parses on `dump`
        // and wins over a config schema_file.
        let cli = parse(&[
            "zeroship-migrate",
            "dump",
            "--database-url",
            "x",
            "--schema-file",
            "/tmp/s.sql",
        ]);
        assert!(matches!(cli.command, Command::Dump));
        assert_eq!(cli.schema_file.as_deref(), Some(Path::new("/tmp/s.sql")));
        let layer = FileEnvLayer {
            schema_file: Some("/from/config.sql".to_string()),
            ..FileEnvLayer::default()
        };
        assert_eq!(
            effective_schema_file(cli.schema_file.as_deref(), &layer),
            PathBuf::from("/tmp/s.sql")
        );
    }

    #[test]
    fn load_is_a_command_with_setup_alias_and_global_schema_file() {
        // `load` parses and accepts the global `--schema-file`.
        let cli = parse(&[
            "zeroship-migrate",
            "load",
            "--database-url",
            "x",
            "--schema-file",
            "/tmp/s.sql",
        ]);
        assert!(matches!(cli.command, Command::Load));
        assert_eq!(cli.schema_file.as_deref(), Some(Path::new("/tmp/s.sql")));
        // The `setup` visible alias resolves to the same command.
        let cli = parse(&["zeroship-migrate", "setup", "--database-url", "x"]);
        assert!(matches!(cli.command, Command::Load));
    }

    #[test]
    fn auto_dump_defaults_on_and_no_dump_schema_turns_it_off() {
        // Default: no flag, empty layer ⇒ auto-dump ON (dbmate parity).
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        assert!(!cli.no_dump_schema);
        assert!(effective_auto_dump(&cli, &empty_layer()));
        // `--no-dump-schema` turns it OFF (one-directional override).
        let cli = parse(&[
            "zeroship-migrate",
            "migrate",
            "--database-url",
            "x",
            "--no-dump-schema",
        ]);
        assert!(cli.no_dump_schema);
        assert!(!effective_auto_dump(&cli, &empty_layer()));
        // A config `dump_schema = false` disables it when the flag is absent.
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        let layer = FileEnvLayer {
            dump_schema: Some(false),
            ..FileEnvLayer::default()
        };
        assert!(!effective_auto_dump(&cli, &layer));
        // The CLI flag still wins to turn it OFF over a config that says ON.
        let cli_off = parse(&[
            "zeroship-migrate",
            "migrate",
            "--database-url",
            "x",
            "--no-dump-schema",
        ]);
        let layer_on = FileEnvLayer {
            dump_schema: Some(true),
            ..FileEnvLayer::default()
        };
        assert!(!effective_auto_dump(&cli_off, &layer_on));
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
        // No flag, no env/config ⇒ the effective dir is the built-in default.
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        assert_eq!(cli.dir, None, "flag is absent");
        assert_eq!(effective_dir(&cli, &empty_layer()), PathBuf::from(DEFAULT_DIR));
    }

    #[test]
    fn dir_precedence_flag_over_layer_over_default() {
        let layer = FileEnvLayer {
            migrations_dir: Some("from_layer".to_string()),
            ..FileEnvLayer::default()
        };
        // No flag ⇒ the layer value wins over the default.
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        assert_eq!(effective_dir(&cli, &layer), PathBuf::from("from_layer"));
        // Flag present ⇒ it wins over the layer.
        let cli_flag = parse(&[
            "zeroship-migrate",
            "migrate",
            "--database-url",
            "x",
            "--dir",
            "from_flag",
        ]);
        assert_eq!(effective_dir(&cli_flag, &layer), PathBuf::from("from_flag"));
    }

    #[test]
    fn trusted_run_config_uses_generic_public_meta_and_single_schema() {
        let cli = parse(&["zeroship-migrate", "migrate", "--database-url", "x"]);
        let cfg = run_config(&cli, &empty_layer()).expect("config builds");
        assert_eq!(cfg.profile, RunProfile::Trusted);
        assert_eq!(cfg.project_schema, DEFAULT_GENERIC_SCHEMA);
        assert_eq!(cfg.meta_schema, DEFAULT_GENERIC_SCHEMA);
        assert_eq!(cfg.schemas, vec![DEFAULT_GENERIC_SCHEMA.to_string()]);
        assert_eq!(cfg.project_id, "default");
        // No --engine ⇒ DSN auto-detect (override is None).
        assert_eq!(cfg.engine_override, None);
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
        let cfg = run_config(&cli, &empty_layer()).expect("config builds");
        assert_eq!(cfg.profile, RunProfile::Platform);
        assert_eq!(cfg.project_schema, "zeroship");
        assert_eq!(cfg.meta_schema, "zeroship_migrations");
        assert!(cfg.schemas.contains(&"oauth_hydra".to_string()));
    }

    #[test]
    fn engine_flag_threads_into_run_config() {
        let cli = parse(&[
            "zeroship-migrate",
            "migrate",
            "--database-url",
            "/bare/path.db",
            "--engine",
            "sqlite",
        ]);
        let cfg = run_config(&cli, &empty_layer()).expect("config builds");
        assert_eq!(cfg.engine_override, Some(EngineKind::Sqlite));
    }

    #[test]
    fn run_config_falls_back_to_config_database_url() {
        // No --database-url and no DATABASE_URL: the file's `database_url` is the
        // lowest fallback (clap leaves cli.database_url None when the env is unset).
        let cli = parse(&["zeroship-migrate", "migrate"]);
        if cli.database_url.is_some() {
            // A DATABASE_URL is set in this environment; skip (can't assert fallback).
            return;
        }
        let layer = FileEnvLayer {
            database_url: Some("sqlite:/tmp/from_config.db".to_string()),
            ..FileEnvLayer::default()
        };
        let cfg = run_config(&cli, &layer).expect("config builds from file DSN");
        assert_eq!(cfg.database_url, "sqlite:/tmp/from_config.db");
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
