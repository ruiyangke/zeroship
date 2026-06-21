//! The OPERATOR-SIDE runner — the single place a [`OperatorCapability`] is minted
//! (design §5 / §9, Phase 3).
//!
//! This module backs the `zeroship-migrate` CLI binary (`src/bin/zeroship-migrate.rs`).
//! The bin's `main` is a THIN arg-parser; it delegates to the `run_*` functions
//! here. Those functions mint the [`OperatorCapability`] token INTERNALLY (via the
//! `pub(super)`-private [`OperatorCapability::new`]) and build the Platform
//! [`GuardConfig`](super::GuardConfig) / [`ExecutorConfig`](crate::db::ExecutorConfig).
//!
//! THE SECURITY-CRITICAL INVARIANT (design §5): the token mint is confined to this
//! module. `submit_migration` / `engine` / any external crate cannot reach it —
//! `new()` is `pub(super)` (visible only within `guard`), the `for_test` seam is
//! `#[cfg(test)]`-only, and `TrustProfile::Platform` is un-nameable externally
//! (`#[non_exhaustive]` + private `GuardConfig` fields). So a Platform guard is a
//! *capability you are granted by holding a token you can only mint here*.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::analyze::Advisory;
use crate::backend::{MigrationBackend, PostgresBackend};
use crate::backend_sqlite::SqliteBackend;
use crate::db::{connect, ConnectError, ExecutorConfig};
use crate::drift::{check_checksum_drift, ChecksumDriftReport, DriftError};
use crate::engine::{EngineError, MigrationEngine, MigrationPlan, RollbackEngineError};
use crate::executor::{
    ApplyOutcome, RollbackError, RollbackOutcome, RollbackRequest, RollbackTarget,
};
use crate::loader::{load_dir, migration_id_for_version, LoaderError};
use crate::migration::MigrationId;
use crate::status::{status, status_via_backend, MigrationStatus, StatusError};
use crate::Approval;

/// A zero-sized Platform capability token. Its `new()` is `pub(super)`, so only
/// the `guard` module (and this submodule, which IS the operator runner) can mint
/// it — the CLI entrypoints below are the only real callers.
///
/// `Clone` is sound: it is a ZST and cloning does not widen authority — you can
/// only clone a token you *already hold*, and you can only hold one by minting it
/// here. It rides on a Platform-built [`super::GuardConfig`] /
/// [`crate::db::ExecutorConfig`] so the executor's internal guard builds can
/// re-derive a Platform `GuardConfig` without a fresh out-of-band mint.
#[derive(Debug, Clone)]
pub(crate) struct OperatorCapability(());

impl OperatorCapability {
    /// The single production mint site, private to the `guard` module
    /// (`pub(super)`). The CLI `run_*` functions below are its only callers; the
    /// creator path (`submit`/`engine`) cannot reach it.
    pub(super) const fn new() -> Self {
        Self(())
    }

    /// **Test-only `pub(crate)` seam.** Lets in-crate guard tests exercise the
    /// Platform profile. This is the ONLY way any module other than the runner can
    /// obtain a token, and it is `#[cfg(test)]`-gated so it does not exist in a
    /// release build — the production mint site stays the `pub(super)` `new` above.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn for_test() -> Self {
        Self::new()
    }
}

/// Mint a [`OperatorCapability`] for the OPERATOR-SIDE shadow dry-run harness
/// ([`crate::shadow`]).
///
/// The shadow harness must mirror the SOURCE config's trust profile so a
/// privileged set dry-runs faithfully: for a Platform source it builds the
/// shadow's own Platform [`ExecutorConfig`] (admin connection, NO migrator
/// `SET ROLE`, §8); for a Trusted source (the public dbmate-like posture) it
/// builds the shadow's own Trusted config (connecting-role apply, deny-list
/// OFF) — both require an operator token. The token mint stays CONFINED to the
/// `guard` module — `OperatorCapability::new()` remains `pub(super)`; this is a
/// single named `pub(crate)` seam the harness (also operator-side, like the CLI
/// runners) calls, rather than widening `new()` itself. The shadow is a throwaway
/// DB the operator owns, so reproducing the source's profile on it is NOT a
/// privilege escalation — it reproduces the real apply on a clone.
#[must_use]
pub(crate) const fn mint_shadow_operator_capability() -> OperatorCapability {
    OperatorCapability::new()
}

/// The trust profile the runner builds its configs under (design §9 `--profile`).
///
/// `Platform` is the binary's default (the CLI is the only place `platform` is
/// selectable — §5); `Confined` is offered for completeness (an operator may run
/// the same binary against a single creator schema with the full deny-list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunProfile {
    /// Trusted operator SQL for the platform schemas — the widened guard (§4).
    Platform,
    /// Untrusted-equivalent: the full deny-list, single-schema.
    Confined,
    /// The public dbmate-like posture (Track A): the operator owns the DB, so the
    /// deny-list is OFF and there is no schema confinement. The DEFAULT of the
    /// public `zeroship-migrate` CLI. Reachable ONLY through the CLI's
    /// `--profile trusted` flag (the single new Trusted surface) — the control
    /// plane uses `submit_migration` (Confined) and never reaches this binary.
    Trusted,
}

/// The operator-supplied inputs for a runner invocation (parsed from the CLI args,
/// design §9). Profile-agnostic; the runner decides token minting from `profile`.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Migration directory (`--dir`, default `db/migrations/`).
    pub dir: std::path::PathBuf,
    /// Admin Postgres DSN (`--database-url` / `DATABASE_URL`).
    pub database_url: String,
    /// Trust profile (`--profile`, default `Platform`).
    pub profile: RunProfile,
    /// The advisory-lock serialization sentinel + journal project id
    /// (`--project-id`, default `"platform"`). Two concurrent `migrate` runs hash
    /// to the same `pg_advisory_lock(hashtext(project_id))` and serialize (§9).
    pub project_id: String,
    /// The primary platform schema, pinned into `search_path` (the first
    /// `--schema`, conventionally `zeroship`).
    pub project_schema: String,
    /// The Platform schema allowlist (`--schema`, repeatable; default
    /// `zeroship`,`oauth_hydra`,`public`).
    pub schemas: Vec<String>,
    /// The `CREATE EXTENSION` allowlist (default `citext`,`uuid-ossp`).
    pub extensions: Vec<String>,
    /// The meta schema holding the append-only journal (`--meta-schema`).
    pub meta_schema: String,
    /// Operator-given destructive approval (`--yes` / `--allow-destructive`).
    pub yes: bool,
    /// Per-statement timeout. Defaults applied by the CLI parser.
    pub statement_timeout: Duration,
    /// Per-statement lock-acquisition timeout.
    pub lock_timeout: Duration,
}

/// What an operator-facing command produced — printable by the bin.
#[derive(Debug)]
pub enum RunReport {
    /// `migrate` applied (or no-op'd) a plan.
    Migrate(ApplyOutcome),
    /// `status` read the journal.
    Status(Box<MigrationStatus>),
    /// `validate` dry-ran + drift-checked (NO DDL on the real DB).
    Validate(Box<ValidateReport>),
    /// `rollback` reversed migrations via `.down.sql`.
    Rollback(RollbackOutcome),
}

/// The result of `validate`: the dry-run report on a shadow DB + checksum drift +
/// the destructive advisories (design §9 `validate`). No DDL touches the real DB.
#[derive(Debug)]
pub struct ValidateReport {
    /// The shadow-DB dry-run outcome (each file applied on a throwaway clone).
    pub dry_run: crate::shadow::DryRunReport,
    /// Checksum / orphan drift of the journal vs. the supplied set.
    pub drift: ChecksumDriftReport,
    /// `true` if the loaded plan is destructive (the H1 gate would refuse `migrate`
    /// without `--yes`).
    pub destructive: bool,
    /// Operational advisories per migration version (lock-heavy ops, drops, …).
    pub advisories: Vec<(String, Vec<Advisory>)>,
}

/// Any failure a runner can surface (printed by the bin; exits non-zero).
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Loading / parsing the migration directory failed.
    #[error("load migrations: {0}")]
    Load(#[from] LoaderError),
    /// Opening the admin connection failed.
    #[error("connect: {0}")]
    Connect(#[from] ConnectError),
    /// The apply gate / executor failed.
    #[error("apply: {0}")]
    Apply(#[from] EngineError),
    /// A status / journal read failed.
    #[error("status: {0}")]
    Status(#[from] StatusError),
    /// A journal bootstrap/read failed directly (the SQLite leg drives the
    /// backend's journal methods, which surface [`JournalError`]).
    #[error("journal: {0}")]
    Journal(#[from] crate::journal::JournalError),
    /// A drift query failed.
    #[error("drift: {0}")]
    Drift(#[from] DriftError),
    /// The shadow dry-run harness failed.
    #[error("dry-run: {0}")]
    DryRun(#[from] crate::shadow::DryRunError),
    /// Rollback failed (the PG engine-driven rollback path).
    #[error("rollback: {0}")]
    Rollback(#[from] RollbackEngineError),
    /// A per-migration rollback failed (the SQLite leg drives
    /// [`MigrationBackend::rollback_one_transactional`] directly).
    #[error("rollback: {0}")]
    RollbackOne(#[from] RollbackError),
    /// The plan is destructive but `--yes` / `--allow-destructive` was not given.
    /// The H1 gate refuses BEFORE any DDL (design §9). Carries the offending
    /// versions + their advisories for the operator to review (run `validate`).
    #[error(
        "refusing to apply a DESTRUCTIVE plan without --yes/--allow-destructive \
         ({} migration(s) flagged); run `validate` to review, then re-run with --yes",
        .versions.len()
    )]
    DestructiveRefused {
        /// The versions the plan flags destructive/approval-gated.
        versions: Vec<String>,
        /// Their operational advisories (so the operator sees what would be lost).
        advisories: Vec<Advisory>,
    },
    /// `wait` exceeded its timeout without the DB accepting a connection.
    #[error("wait: database not reachable within {secs}s (last error: {last_error})")]
    WaitTimeout {
        /// The timeout budget that elapsed, in seconds.
        secs: u64,
        /// The most recent connection/probe error seen before timing out.
        last_error: String,
    },
    /// The DB URL selects an engine this CLI does not support (e.g. `mysql://`).
    /// An HONEST refusal, not a panic — the engine abstraction is general, but
    /// only the Postgres + SQLite backends are wired into this binary.
    #[error(
        "engine not supported by this CLI (Postgres and SQLite supported; MySQL needs \
         the SqlDialect::MySql variant + a MySQL backend — see the multi-engine design)"
    )]
    UnsupportedEngine,
    /// A SQLite backend op (open / hardening / apply / journal) failed.
    #[error("sqlite: {0}")]
    Sqlite(String),
    /// A command is not supported on the SQLite leg of this CLI (`dump` =
    /// `pg_dump`, PG-only). An explicit, honest refusal — never a fake success.
    #[error("{0} is not supported on SQLite via this CLI (Postgres-only)")]
    UnsupportedOnSqlite(&'static str),
}

/// The H1 destructive-gate decision (design §9, pure + unit-testable).
///
/// Returns the [`Approval`] to forward to the engine, OR an error refusing the
/// apply. The engine + executor independently re-check `Approval`
/// (`executor.rs:596-600`), so this is the operator-facing confirmation layer, not
/// the only gate.
///
/// - plan NOT destructive (and not `requires_approval`) ⇒ `Approved` (additive
///   fresh-DB apply is the common path; no `--yes` needed);
/// - plan destructive + `yes` given ⇒ `Approved`;
/// - plan destructive + `yes` NOT given ⇒ refuse (`DestructiveRefused`), nothing
///   applies.
///
/// # Errors
/// [`RunError::DestructiveRefused`] when the plan is destructive (or
/// approval-gated) and `yes` is `false`.
pub fn destructive_gate_decision(
    plan: &MigrationPlan,
    yes: bool,
) -> Result<Approval, RunError> {
    let gated = plan.destructive || plan.requires_approval;
    if !gated {
        return Ok(Approval::Approved);
    }
    if yes {
        return Ok(Approval::Approved);
    }
    // Destructive + not approved: refuse with the offending versions + advisories.
    let mut versions = Vec::new();
    let mut advisories = Vec::new();
    for item in &plan.items {
        if item.report.destructive
            || item.migration.flags.destructive
            || item.migration.flags.requires_approval
        {
            versions.push(item.migration.version.as_str().to_string());
        }
        advisories.extend(item.report.advisories.iter().cloned());
    }
    Err(RunError::DestructiveRefused {
        versions,
        advisories,
    })
}

/// The DB engine the CLI dispatches to, selected by the `--database-url` shape
/// (multi-engine abstraction P7 — the public CLI is now multi-engine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    /// `postgres://` / `postgresql://` — the existing PG path (byte-identical).
    Postgres,
    /// `sqlite:` / `sqlite://` / `file:` / `:memory:` / a bare filesystem path —
    /// the hardened [`SqliteBackend`] on that file. Carries the app-file path.
    Sqlite(PathBuf),
    /// Any other explicit scheme (`mysql://`, `redis://`, …) — unsupported by this
    /// binary. The engine abstraction is general; only PG + SQLite are wired here.
    Unsupported,
}

/// Classify a `--database-url` into the [`Engine`] this CLI dispatches to, using
/// the SAME single-source grammar as `zeroship_core::db_url::is_sqlite_url` /
/// plugin-db's `backend_for_url` (no drift between the opener and the CLI).
///
/// - `postgres://` / `postgresql://` ⇒ [`Engine::Postgres`].
/// - `sqlite:` / `sqlite://` / `file:` / `:memory:` / a bare path ⇒
///   [`Engine::Sqlite`] with the extracted file path.
/// - an explicit but unrecognised scheme (e.g. `mysql://`) ⇒ [`Engine::Unsupported`].
#[must_use]
pub fn classify_engine(url: &str) -> Engine {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        return Engine::Postgres;
    }
    // libpq **keyword/value** DSN (`host=… port=… dbname=…`) — the form the
    // existing PG callers (compose/ops, the `cli_platform_pg` suite) pass. The
    // `postgres://`-only `is_sqlite_url` would mis-read this as a bare filesystem
    // path (SQLite), so classify it as Postgres FIRST — this is what keeps the PG
    // path byte-identical for keyword DSNs.
    if is_libpq_keyword_dsn(trimmed) {
        return Engine::Postgres;
    }
    // The shared classifier: a bare path is SQLite; an unknown scheme is not.
    if zeroship_core::db_url::is_sqlite_url(trimmed) {
        return Engine::Sqlite(sqlite_file_path(trimmed));
    }
    Engine::Unsupported
}

/// `true` iff `dsn` is a libpq **keyword/value** connection string — at least one
/// whitespace-separated `key=value` token whose key is a known PG connection
/// parameter (`host`, `hostaddr`, `port`, `dbname`, `user`, `password`,
/// `sslmode`, …). This is the non-URI DSN form `compio_postgres::connect` accepts
/// and the existing PG CLI callers use; a SQLite DSN / bare path never looks like
/// this (a Unix path has no `key=value` PG-parameter token).
fn is_libpq_keyword_dsn(dsn: &str) -> bool {
    const PG_KEYS: &[&str] = &[
        "host",
        "hostaddr",
        "port",
        "dbname",
        "user",
        "password",
        "sslmode",
        "connect_timeout",
        "application_name",
        "options",
        "passfile",
        "service",
        "target_session_attrs",
    ];
    dsn.split_whitespace().any(|tok| {
        tok.split_once('=')
            .is_some_and(|(k, _)| PG_KEYS.contains(&k.to_ascii_lowercase().as_str()))
    })
}

/// Extract the SQLite app-file path from a SQLite DSN, mirroring plugin-db's
/// `backend_for_url` exactly (`sqlite://host/db` folds the authority into the
/// path; `sqlite:` / `file:` strip the scheme; `:memory:` and a bare path pass
/// through verbatim). Only called for URLs `is_sqlite_url` already accepted.
fn sqlite_file_path(url: &str) -> PathBuf {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower == ":memory:" {
        return PathBuf::from(":memory:");
    }
    if lower.starts_with("sqlite://") {
        return PathBuf::from(&trimmed["sqlite://".len()..]);
    }
    if lower.starts_with("sqlite:") {
        return PathBuf::from(&trimmed["sqlite:".len()..]);
    }
    if lower.starts_with("file:") {
        return PathBuf::from(&trimmed["file:".len()..]);
    }
    // A bare filesystem path.
    PathBuf::from(trimmed)
}

/// The journal-file path for a SQLite app file: a sibling `<file>.migrations`
/// suffix (the dbmate-style separate `_mig` journal database, design §2.5.2 —
/// engine-constructed, never creator input). For `:memory:` the journal is the
/// matching `:memory:` (a throwaway-by-nature dev DB).
fn sqlite_journal_path(app: &Path) -> PathBuf {
    if app.as_os_str() == ":memory:" {
        return PathBuf::from(":memory:");
    }
    let mut s = app.to_path_buf().into_os_string();
    s.push(".migrations");
    PathBuf::from(s)
}

/// Open the hardened [`SqliteBackend`] for the CLI's SQLite leg, from the app-file
/// path extracted off the `--database-url`. The journal lives in a sibling
/// `<file>.migrations` file (§2.5.2).
fn open_sqlite_backend(app: &Path) -> Result<SqliteBackend, RunError> {
    let journal = sqlite_journal_path(app);
    SqliteBackend::open(app, &journal).map_err(|e| RunError::Sqlite(e.to_string()))
}

/// Build the [`ExecutorConfig`] for a run, minting the [`OperatorCapability`]
/// internally when `profile == Platform`. THIS is the confined token mint.
fn build_exec_cfg(cfg: &RunConfig) -> ExecutorConfig {
    let mut exec = match cfg.profile {
        RunProfile::Platform => {
            // The single token mint (design §5). No other module can reach `new()`.
            let cap = OperatorCapability::new();
            ExecutorConfig::platform(
                &cap,
                cfg.project_id.clone(),
                cfg.project_schema.clone(),
                cfg.schemas.clone(),
                cfg.extensions.clone(),
            )
        }
        RunProfile::Trusted => {
            // The single token mint (design §5), shared with Platform. No other
            // module can reach `new()`.
            let cap = OperatorCapability::new();
            ExecutorConfig::trusted(&cap, cfg.project_id.clone(), cfg.project_schema.clone())
        }
        RunProfile::Confined => ExecutorConfig::new(cfg.project_id.clone(), cfg.project_schema.clone()),
    };
    exec.pg.meta_schema.clone_from(&cfg.meta_schema);
    exec.pg.statement_timeout = cfg.statement_timeout;
    exec.pg.lock_timeout = cfg.lock_timeout;
    // Platform applies run as the admin connection (no SET ROLE) — design §8.
    // `migrator_role` stays `None` (the `ExecutorConfig::new`/`platform` default).
    exec
}

/// The planner [`GuardConfig`] mirroring `exec`'s profile. Minted via the same
/// token path so the plan is not re-denied by the executor's internal guard.
fn build_guard_cfg(cfg: &RunConfig) -> super::GuardConfig {
    match cfg.profile {
        RunProfile::Platform => {
            let cap = OperatorCapability::new();
            super::GuardConfig::platform(&cap, cfg.schemas.clone(), cfg.extensions.clone())
        }
        RunProfile::Trusted => {
            let cap = OperatorCapability::new();
            super::GuardConfig::trusted(&cap)
        }
        RunProfile::Confined => super::GuardConfig::confined(cfg.project_schema.clone()),
    }
}

/// The SQLite [`ExecutorConfig`] for the CLI's SQLite leg. SQLite ignores the PG
/// schema/role machinery (the single migration actor serializes structurally, the
/// `_mig` journal lives in the attached file), so a plain
/// [`ExecutorConfig::new`](ExecutorConfig::new) keyed on the project id/schema is
/// all the engine needs to thread. The CLI's `--profile` is effectively
/// Trusted/Confined for an operator-owned local file; the SQLite authorizer (line-2)
/// is intrinsic to [`SqliteBackend`] regardless of profile.
fn sqlite_exec_cfg(cfg: &RunConfig) -> ExecutorConfig {
    ExecutorConfig::new(cfg.project_id.clone(), cfg.project_schema.clone())
}

/// The SQLite planner [`GuardConfig`]. The line-1 vet on SQLite is the descriptor
/// guard (a clean outcome for trusted descriptor/dbmate DDL — `libpg_query` cannot
/// vet SQLite); the real confinement is the backend authorizer at apply (line-2).
fn sqlite_guard_cfg(cfg: &RunConfig) -> super::GuardConfig {
    super::GuardConfig::confined_sqlite(cfg.project_schema.clone())
}

/// `migrate` — dispatch by the `--database-url` engine, then load the dir, plan,
/// honour the H1 destructive gate, and apply PENDING (design §9). Postgres runs
/// the existing byte-identical path; SQLite runs the same generic engine through
/// a hardened [`SqliteBackend`]; an unsupported URL is an honest refusal.
///
/// # Errors
/// [`RunError`] on an unsupported engine / load / connect / a
/// destructive-without-`--yes` refusal / apply.
pub async fn run_migrate(cfg: &RunConfig) -> Result<RunReport, RunError> {
    match classify_engine(&cfg.database_url) {
        Engine::Postgres => run_migrate_pg(cfg).await,
        Engine::Sqlite(app) => run_migrate_sqlite(cfg, &app).await,
        Engine::Unsupported => Err(RunError::UnsupportedEngine),
    }
}

/// The SQLite leg of `migrate`: plan with the SQLite guard, gate destructive, then
/// apply through the hardened [`SqliteBackend`] via the SAME generic engine path.
async fn run_migrate_sqlite(cfg: &RunConfig, app: &Path) -> Result<RunReport, RunError> {
    let migrations = load_dir(&cfg.dir)?;
    let backend = open_sqlite_backend(app)?;
    let exec_cfg = sqlite_exec_cfg(cfg);
    let guard_cfg = sqlite_guard_cfg(cfg);
    let engine = MigrationEngine::new();
    let plan = engine.plan(&migrations, &guard_cfg);
    let approval = destructive_gate_decision(&plan, cfg.yes)?;
    let outcome = engine
        .apply(&plan, approval, &backend, &exec_cfg, "platform-migrate")
        .await?;
    Ok(RunReport::Migrate(outcome))
}

/// The Postgres leg of `migrate` — the existing path, byte-identical.
async fn run_migrate_pg(cfg: &RunConfig) -> Result<RunReport, RunError> {
    let migrations = load_dir(&cfg.dir)?;
    let conn = connect(&cfg.database_url).await?;
    let exec_cfg = build_exec_cfg(cfg);
    let guard_cfg = build_guard_cfg(cfg);
    let engine = MigrationEngine::new();
    let plan = engine.plan(&migrations, &guard_cfg);
    // H1: refuse a destructive plan without --yes; only then forward Approved.
    let approval = destructive_gate_decision(&plan, cfg.yes)?;
    let outcome = engine
        .apply(
            &plan,
            approval,
            &PostgresBackend::new(&conn),
            &exec_cfg,
            "platform-migrate",
        )
        .await?;
    Ok(RunReport::Migrate(outcome))
}

/// `status` — read the journal, print applied vs pending (+ rolled-back). NO DDL
/// beyond the journal's idempotent bootstrap (design §9). Dispatches by engine:
/// the PG `&Client` snapshot path on Postgres, the backend-generic
/// [`status_via_backend`] on SQLite.
///
/// # Errors
/// [`RunError`] on an unsupported engine / load / connect / a journal read failure.
pub async fn run_status(cfg: &RunConfig) -> Result<RunReport, RunError> {
    let migrations = load_dir(&cfg.dir)?;
    let st = match classify_engine(&cfg.database_url) {
        Engine::Postgres => {
            let conn = connect(&cfg.database_url).await?;
            let exec_cfg = build_exec_cfg(cfg);
            status(&conn, &exec_cfg, &migrations).await?
        }
        Engine::Sqlite(app) => {
            let backend = open_sqlite_backend(&app)?;
            let exec_cfg = sqlite_exec_cfg(cfg);
            status_via_backend(&backend, &exec_cfg, &migrations).await?
        }
        Engine::Unsupported => return Err(RunError::UnsupportedEngine),
    };
    Ok(RunReport::Status(Box::new(st)))
}

/// `validate` — dry-run every file on a SHADOW DB + report checksum drift against
/// the journal + surface destructive advisories. NO DDL on the real DB (design §9;
/// `update-sql` + `validate`).
///
/// Postgres-only: `validate` is shadow-dry-run-driven, and the SQLite backend
/// DELIBERATELY exposes no `ShadowDryRun` capability (`shadow()` is `None` — SQLite
/// DDL is trusted descriptor output and dev is recoverable, so a pre-apply shadow
/// clone adds nothing; the engine would surface the explicit `ShadowUnsupported`).
/// On SQLite this refuses honestly rather than reporting a fake dry-run pass.
///
/// # Errors
/// [`RunError`] on an unsupported engine / load / connect / the shadow harness / a
/// drift read failure.
pub async fn run_validate(cfg: &RunConfig) -> Result<RunReport, RunError> {
    match classify_engine(&cfg.database_url) {
        Engine::Postgres => {}
        Engine::Sqlite(_) => return Err(RunError::UnsupportedOnSqlite("validate")),
        Engine::Unsupported => return Err(RunError::UnsupportedEngine),
    }
    let migrations = load_dir(&cfg.dir)?;
    let conn = connect(&cfg.database_url).await?;
    let exec_cfg = build_exec_cfg(cfg);
    let guard_cfg = build_guard_cfg(cfg);
    let engine = MigrationEngine::new();

    // The plan tells us destructiveness + advisories WITHOUT touching the DB.
    let plan = engine.plan(&migrations, &guard_cfg);
    let advisories: Vec<(String, Vec<Advisory>)> = plan
        .items
        .iter()
        .map(|p| {
            (
                p.migration.version.as_str().to_string(),
                p.report.advisories.clone(),
            )
        })
        .collect();

    // Dry-run on a THROWAWAY shadow database (CREATE/DROP DATABASE on a clone) —
    // the real DB sees no migration DDL.
    let shadow_cfg = crate::shadow::ShadowConfig {
        admin_dsn: cfg.database_url.clone(),
        db_name_prefix: "zsmig_shadow_".to_string(),
    };
    let dry_run = engine
        .dry_run(
            &PostgresBackend::new(&conn),
            &migrations,
            &exec_cfg,
            &shadow_cfg,
            "platform-validate",
        )
        .await?;

    // Checksum drift against the REAL journal (read-only). Bootstrap the journal
    // first — the SAME idempotent `CREATE … IF NOT EXISTS` meta-schema bootstrap
    // `status` performs (so a fresh DB drift-checks cleanly). This touches ONLY
    // the meta schema; NO migration DDL hits the project schema (the §9 "no DDL on
    // the real DB" guarantee is about migration DDL — the drift read needs the
    // journal to exist, exactly as `status` does).
    crate::journal::ensure_journal(&conn, &exec_cfg)
        .await
        .map_err(crate::status::StatusError::Journal)?;
    let drift = check_checksum_drift(&conn, &exec_cfg, &migrations).await?;

    Ok(RunReport::Validate(Box::new(ValidateReport {
        dry_run,
        drift,
        destructive: plan.destructive || plan.requires_approval,
        advisories,
    })))
}

/// `rollback` — reverse applied migrations via their `.down.sql` to a target
/// version (or N steps), GATED on `--yes` (rollback is always destructive, design
/// §9 / `engine.rs:719`).
///
/// `to_version` rolls back everything strictly after the given numeric version;
/// `steps` rolls back the N most-recent. Exactly one is honoured (`to_version`
/// wins if both are set, matching the CLI's mutually-exclusive flags).
///
/// # Errors
/// [`RunError`] on load / connect / a missing `--yes` / the executor's rollback.
pub async fn run_rollback(
    cfg: &RunConfig,
    to_version: Option<u64>,
    steps: Option<usize>,
) -> Result<RunReport, RunError> {
    // Rollback is ALWAYS destructive ⇒ require explicit --yes (mirrors the engine
    // gate; the engine + executor re-check Approval as defense in depth).
    if !cfg.yes {
        return Err(RunError::DestructiveRefused {
            versions: Vec::new(),
            advisories: Vec::new(),
        });
    }
    let target = match (to_version, steps) {
        (Some(v), _) => RollbackTarget::ToVersion(migration_id_for_version(v)),
        (None, Some(n)) => RollbackTarget::Steps(n),
        (None, None) => RollbackTarget::All,
    };

    match classify_engine(&cfg.database_url) {
        Engine::Postgres => run_rollback_pg(cfg, target).await,
        Engine::Sqlite(app) => run_rollback_sqlite(cfg, &app, target).await,
        Engine::Unsupported => Err(RunError::UnsupportedEngine),
    }
}

/// The Postgres leg of `rollback` — the existing path, byte-identical.
async fn run_rollback_pg(cfg: &RunConfig, target: RollbackTarget) -> Result<RunReport, RunError> {
    let migrations = load_dir(&cfg.dir)?;
    let conn = connect(&cfg.database_url).await?;
    let exec_cfg = build_exec_cfg(cfg);
    let engine = MigrationEngine::new();
    let request = RollbackRequest::new(target);
    let outcome = engine
        .rollback(
            &migrations,
            request,
            Approval::Approved,
            &conn,
            &exec_cfg,
            "platform-rollback",
        )
        .await?;
    Ok(RunReport::Rollback(outcome))
}

/// The SQLite leg of `rollback`: select the target versions from the net-applied
/// `_mig` journal and reverse each via [`MigrationBackend::rollback_one_transactional`]
/// (the additive `down` + `_rolled_back` append, atomic on the single actor) in
/// reverse-version order — the SQLite peer of the PG executor's rollback. SQLite
/// has no `depends_on` graph on the CLI's flat dbmate set, so reverse-version order
/// IS reverse-apply order. A rebuild-needing `down` surfaces the backend's explicit
/// `SqliteRebuildRequired` error (the CLI does not auto-rebuild on rollback).
async fn run_rollback_sqlite(
    cfg: &RunConfig,
    app: &Path,
    target: RollbackTarget,
) -> Result<RunReport, RunError> {
    let migrations = load_dir(&cfg.dir)?;
    let backend = open_sqlite_backend(app)?;
    let exec_cfg = sqlite_exec_cfg(cfg);
    backend.ensure_journal(&exec_cfg).await?;

    // Net-applied versions (latest event = completed), highest version first.
    let entries = backend.applied(&exec_cfg).await?;
    let mut applied: Vec<MigrationId> = entries
        .iter()
        .filter(|e| e.phase == crate::journal::Phase::Completed)
        .filter_map(|e| MigrationId::parse(&e.version).ok())
        .collect();
    applied.sort();
    applied.reverse(); // most-recent (highest version) first.

    // Which net-applied versions does the target select? (Same semantics as the PG
    // RollbackTarget: ToVersion keeps the target, Steps takes the N most-recent.)
    let selected: Vec<MigrationId> = match &target {
        RollbackTarget::All => applied.clone(),
        RollbackTarget::Steps(n) => applied.iter().take(*n).cloned().collect(),
        RollbackTarget::ToVersion(v) => {
            applied.iter().filter(|a| *a > v).cloned().collect()
        }
    };

    // Index loaded migrations by version so each selected version's `down` is found.
    let by_version: std::collections::HashMap<&str, &crate::migration::Migration> =
        migrations.iter().map(|m| (m.version.as_str(), m)).collect();

    let mut rolled_back = Vec::new();
    for version in &selected {
        let Some(m) = by_version.get(version.as_str()) else {
            return Err(RunError::Sqlite(format!(
                "applied version {} has no migration file in --dir to source its `down`",
                version.as_str()
            )));
        };
        backend
            .rollback_one_transactional(&exec_cfg, m, "platform-rollback")
            .await?;
        rolled_back.push(version.as_str().to_string());
    }

    Ok(RunReport::Rollback(RollbackOutcome {
        rolled_back,
        skipped_irreversible: Vec::new(),
    }))
}

/// `down` — roll back the SINGLE most-recently-applied migration (dbmate `down`
/// semantics: one step), GATED on `--yes` (rollback is always destructive).
///
/// This is the dbmate-parity peer of [`run_rollback`]: where `rollback` takes a
/// `--to`/`--steps` target, `down` is hard-wired to `Steps(1)` — undo exactly the
/// last applied migration.
///
/// # Errors
/// [`RunError`] on load / connect / a missing `--yes` / the executor's rollback.
pub async fn run_down(cfg: &RunConfig) -> Result<RunReport, RunError> {
    // `down` is one-step rollback; delegate to the shared rollback path with a
    // `Steps(1)` target so the --yes gate + executor re-checks are identical.
    run_rollback(cfg, None, Some(1)).await
}

/// `--lint` — return the per-migration operational advisories for the loaded set,
/// WITHOUT touching the DB. Non-blocking: advisories never deny (they are
/// [`analyze`](crate::analyze::analyze) heads-ups, the value-add for the
/// generic/Trusted mode where the deny-list is off).
///
/// Plans under the run's profile (so the guard report carries the advisories) and
/// extracts `(version, advisories)`. Pure with respect to the database — it never
/// connects; the CLI calls it after a `migrate`/`validate` to print the footguns.
///
/// # Errors
/// [`RunError`] only on a load/parse failure of the migration directory.
pub fn lint_advisories(cfg: &RunConfig) -> Result<Vec<(String, Vec<Advisory>)>, RunError> {
    let migrations = load_dir(&cfg.dir)?;
    let guard_cfg = build_guard_cfg(cfg);
    let engine = MigrationEngine::new();
    let plan = engine.plan(&migrations, &guard_cfg);
    Ok(plan
        .items
        .iter()
        .map(|p| {
            (
                p.migration.version.as_str().to_string(),
                p.report.advisories.clone(),
            )
        })
        .collect())
}

/// `wait` — poll the DSN until the database accepts a connection (a successful
/// `SELECT 1`), or time out (dbmate `wait`).
///
/// Retries [`connect`] + a trivial `SELECT 1` on a short interval until the DB is
/// reachable or `timeout` elapses. Returns `Ok(())` as soon as the DB answers;
/// the bin exits 0. On timeout it returns [`RunError::WaitTimeout`] and the bin
/// exits non-zero.
///
/// # Errors
/// [`RunError::WaitTimeout`] if the DB is not reachable within `timeout`.
pub async fn run_wait(
    database_url: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<(), RunError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let last_err = match probe_once(database_url).await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if std::time::Instant::now() >= deadline {
            return Err(RunError::WaitTimeout {
                secs: timeout.as_secs(),
                last_error: last_err,
            });
        }
        // Short async sleep on the compio timer (zero tokio) before retrying.
        compio::time::sleep(interval).await;
    }
}

/// One `wait` probe, dispatched by engine. Postgres: connect + `SELECT 1` (the
/// existing probe, byte-identical). SQLite: the file is openable as a hardened
/// backend (the dev-file analog of "the DB accepts a connection" — `:memory:` is
/// always ready). An unsupported URL fails the probe with a clear message (so
/// `wait` times out honestly rather than panicking).
async fn probe_once(database_url: &str) -> Result<(), String> {
    match classify_engine(database_url) {
        Engine::Postgres => {
            let conn = connect(database_url).await.map_err(|e| e.to_string())?;
            conn.query("SELECT 1", &[])
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Engine::Sqlite(app) => {
            // The SQLite "is it ready?" probe: can we open the hardened backend on
            // the file? (`:memory:` always opens.) No journal mutation — the open
            // path hardens the connection but does not bootstrap the journal.
            open_sqlite_backend(&app).map(|_| ()).map_err(|e| e.to_string())
        }
        Engine::Unsupported => Err(RunError::UnsupportedEngine.to_string()),
    }
}

/// Convenience for the bin: the conventional Platform schema allowlist default.
#[must_use]
pub fn default_platform_schemas() -> Vec<String> {
    vec![
        "zeroship".to_string(),
        "oauth_hydra".to_string(),
        "public".to_string(),
    ]
}

/// Convenience for the bin: the conventional Platform extension allowlist default.
#[must_use]
pub fn default_platform_extensions() -> Vec<String> {
    vec!["citext".to_string(), "uuid-ossp".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::PlannedMigration;
    use crate::migration::{Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId};
    use super::super::{GuardOutcome, TrustProfile};

    fn mk_migration(version: u64, destructive: bool, requires_approval: bool) -> Migration {
        let flags = MigrationFlags {
            destructive,
            requires_approval,
            ..MigrationFlags::default()
        };
        let mut m = Migration {
            version: migration_id_for_version(version),
            name: format!("m{version}"),
            up: "CREATE TABLE zeroship.t (id int)".to_string(),
            down: None,
            checksum: Checksum::of(&ChecksumInput {
                up: "",
                down: None,
                flags: &MigrationFlags::default(),
                owner_app: "",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            }),
            flags,
            owner_app: "platform".to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        };
        m.recompute_checksum();
        m
    }

    fn plan_with(destructive: bool, requires_approval: bool) -> MigrationPlan {
        let m = mk_migration(1, destructive, requires_approval);
        MigrationPlan {
            items: vec![PlannedMigration {
                migration: m,
                report: GuardOutcome {
                    destructive,
                    advisories: Vec::new(),
                },
            }],
            destructive,
            requires_approval,
            denied: Vec::new(),
        }
    }

    // ---- H1 destructive-gate decision (pure, no DB) ------------------------

    #[test]
    fn h1_non_destructive_plan_approves_without_yes() {
        let plan = plan_with(false, false);
        let approval = destructive_gate_decision(&plan, false).expect("non-destructive approves");
        assert_eq!(approval, Approval::Approved);
    }

    #[test]
    fn h1_destructive_plan_without_yes_is_refused() {
        let plan = plan_with(true, false);
        let err = destructive_gate_decision(&plan, false)
            .expect_err("destructive without --yes must refuse");
        match err {
            RunError::DestructiveRefused { versions, .. } => {
                assert_eq!(versions.len(), 1, "the one destructive version is named");
            }
            other => panic!("expected DestructiveRefused, got {other:?}"),
        }
    }

    #[test]
    fn h1_destructive_plan_with_yes_proceeds() {
        let plan = plan_with(true, false);
        let approval =
            destructive_gate_decision(&plan, true).expect("destructive + --yes approves");
        assert_eq!(approval, Approval::Approved);
    }

    #[test]
    fn h1_requires_approval_only_without_yes_is_refused() {
        // requires_approval without the SQL-text destructive flag still gates.
        let plan = plan_with(false, true);
        let err = destructive_gate_decision(&plan, false)
            .expect_err("requires_approval without --yes must refuse");
        assert!(matches!(err, RunError::DestructiveRefused { .. }));
    }

    #[test]
    fn h1_requires_approval_with_yes_proceeds() {
        let plan = plan_with(false, true);
        let approval = destructive_gate_decision(&plan, true).expect("requires_approval + --yes");
        assert_eq!(approval, Approval::Approved);
    }

    // ---- the runner mints a Platform config (token confined here) ----------

    #[test]
    fn build_exec_cfg_platform_is_platform_trust() {
        let cfg = RunConfig {
            dir: std::path::PathBuf::from("db/migrations"),
            database_url: "host=localhost".to_string(),
            profile: RunProfile::Platform,
            project_id: "platform".to_string(),
            project_schema: "zeroship".to_string(),
            schemas: default_platform_schemas(),
            extensions: default_platform_extensions(),
            meta_schema: "zeroship_migrations".to_string(),
            yes: false,
            statement_timeout: Duration::from_secs(60),
            lock_timeout: Duration::from_secs(30),
        };
        let exec = build_exec_cfg(&cfg);
        assert_eq!(
            exec.guard_config().trust(),
            TrustProfile::Platform,
            "the runner mints a Platform executor config"
        );
        assert!(exec.pg.migrator_role.is_none(), "Platform runs as admin (no SET ROLE)");
        let guard = build_guard_cfg(&cfg);
        assert_eq!(guard.trust(), TrustProfile::Platform);
    }

    #[test]
    fn build_exec_cfg_confined_is_confined_trust() {
        let cfg = RunConfig {
            dir: std::path::PathBuf::from("db/migrations"),
            database_url: "host=localhost".to_string(),
            profile: RunProfile::Confined,
            project_id: "proj_x".to_string(),
            project_schema: "proj_x".to_string(),
            schemas: vec!["proj_x".to_string()],
            extensions: Vec::new(),
            meta_schema: "meta_x".to_string(),
            yes: false,
            statement_timeout: Duration::from_secs(60),
            lock_timeout: Duration::from_secs(30),
        };
        let exec = build_exec_cfg(&cfg);
        assert_eq!(
            exec.guard_config().trust(),
            TrustProfile::Confined
        );
    }

    // A sanity check that the id-for-version helper round-trips for rollback.
    #[test]
    fn rollback_target_version_round_trips() {
        let id: MigrationId = migration_id_for_version(25);
        assert!(MigrationId::parse(id.as_str()).is_ok());
    }

    // ---- multi-engine URL dispatch (P7) ------------------------------------

    #[test]
    fn classify_engine_routes_postgres_sqlite_and_unsupported() {
        assert_eq!(
            classify_engine("postgres://u:p@host:5432/db"),
            Engine::Postgres
        );
        assert_eq!(classify_engine("postgresql://host/db"), Engine::Postgres);
        // The libpq keyword/value DSN form (the existing PG callers) is Postgres,
        // NOT a SQLite bare path — the byte-identical guard for keyword DSNs.
        assert_eq!(
            classify_engine(
                "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test"
            ),
            Engine::Postgres
        );
        assert_eq!(classify_engine("dbname=mydb"), Engine::Postgres);

        // SQLite schemes + a bare path all route to the SQLite leg with the file.
        assert_eq!(
            classify_engine("sqlite:/tmp/dev.sqlite"),
            Engine::Sqlite(PathBuf::from("/tmp/dev.sqlite"))
        );
        assert_eq!(
            classify_engine("sqlite://./data/app.sqlite"),
            Engine::Sqlite(PathBuf::from("./data/app.sqlite"))
        );
        assert_eq!(
            classify_engine("file:./local.db"),
            Engine::Sqlite(PathBuf::from("./local.db"))
        );
        assert_eq!(
            classify_engine(":memory:"),
            Engine::Sqlite(PathBuf::from(":memory:"))
        );
        assert_eq!(
            classify_engine("/var/lib/zeroship/dev.sqlite"),
            Engine::Sqlite(PathBuf::from("/var/lib/zeroship/dev.sqlite"))
        );

        // An explicit unknown scheme is the honest unsupported refusal.
        assert_eq!(classify_engine("mysql://localhost/db"), Engine::Unsupported);
        assert_eq!(classify_engine("redis://localhost:6379"), Engine::Unsupported);
    }

    #[test]
    fn sqlite_journal_path_is_a_sibling_migrations_file() {
        assert_eq!(
            sqlite_journal_path(&PathBuf::from("/tmp/app.sqlite")),
            PathBuf::from("/tmp/app.sqlite.migrations")
        );
        // `:memory:` journals to a matching in-memory DB (throwaway by nature).
        assert_eq!(
            sqlite_journal_path(&PathBuf::from(":memory:")),
            PathBuf::from(":memory:")
        );
    }
}
