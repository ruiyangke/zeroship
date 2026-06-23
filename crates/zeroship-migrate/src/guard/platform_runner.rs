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
/// `Trusted` is the public CLI's DEFAULT (`--profile trusted`, the binary default):
/// the operator owns the DB, so the deny-list is OFF and there is no schema
/// confinement. `Platform` (the widened guard for the zeroship platform schemas)
/// and `Confined` (the full creator deny-list, single-schema) are EXPLICIT opt-ins
/// the CLI selects only when `--profile platform`/`confined` is passed — the
/// control plane uses `submit_migration` (Confined) and never reaches this binary.
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
    /// An explicit engine override (`--engine pg|sqlite`). When `Some`, it
    /// overrides the DSN-shape auto-detection in [`RunConfig::resolve_engine`] (and
    /// errors if it contradicts an unambiguous DSN scheme). When `None`, the engine
    /// is auto-detected from the DSN shape (`classify_engine`) — the default.
    pub engine_override: Option<EngineKind>,
    /// Trust profile (`--profile`, default `Trusted` — the public CLI default).
    pub profile: RunProfile,
    /// The advisory-lock serialization sentinel + journal project id
    /// (`--project-id`, default `"default"`). Two concurrent `migrate` runs hash
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
    /// `load` (a.k.a. `db:setup`) restored a dumped schema + reconstructed the journal.
    Load(LoadOutcome),
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
    /// The DB URL selects a database engine this CLI does not support. An HONEST
    /// refusal, not a panic — only the Postgres + SQLite backends are supported.
    #[error("unsupported database engine (only postgres and sqlite are supported)")]
    UnsupportedEngine,
    /// An explicit `--engine` (or `ZEROSHIP_MIGRATE_ENGINE` / config `engine`)
    /// contradicts the engine the DSN's unambiguous scheme implies. We refuse
    /// rather than silently trusting one side.
    #[error(
        "engine conflict: --engine {requested} contradicts the {dsn} scheme of the \
         database URL; drop --engine (the DSN already selects the engine) or fix the URL"
    )]
    EngineConflict {
        /// The engine the operator asked for explicitly.
        requested: &'static str,
        /// The engine the DSN's unambiguous scheme implies.
        dsn: &'static str,
    },
    /// A SQLite backend op (open / hardening / apply / journal) failed.
    #[error("sqlite: {0}")]
    Sqlite(String),
    /// `load` (a.k.a. `db:setup`) was refused or failed: the schema file is
    /// missing/unreadable, the target DB is already engine-managed (first-entry-only —
    /// `load` must not clobber an existing DB), or restoring the dump DDL failed.
    #[error("load: {0}")]
    LoadCmd(String),
    /// A loaded migration plan was not a single-step DDL plan (`op.*` DSL §5.2):
    /// the platform Flyway-mode runner consumes plans through the thin
    /// `AppliedPlan::single_step_migration()` facade, which fails closed on a
    /// multi-step plan. This arm is **provably unreachable on the platform path**
    /// (a Flyway/dbmate `.sql` always lowers to one `Ddl` step — asserted by the
    /// single-step-shape precondition test) and exists for defense in depth.
    #[error("plan shape: {0}")]
    NotSingleStep(#[from] crate::plan::NotSingleStep),
}

/// Load the migration directory and project each [`AppliedPlan`] down to its one
/// `&Migration` through the [`single_step_migration`](crate::plan::AppliedPlan::single_step_migration)
/// facade (`op.*` DSL §5.2). The platform Flyway-mode runner operates over
/// `Migration` and never touches `PlanStep`/`RenameStep`, so it stays decoupled
/// from plan-shape evolution; the facade fails closed if a platform changelog
/// ever produced a multi-step plan (it cannot today — see the precondition test).
fn load_dir_flat(dir: &Path) -> Result<Vec<crate::migration::Migration>, RunError> {
    let plans = load_dir(dir)?;
    let mut migrations = Vec::with_capacity(plans.len());
    for plan in &plans {
        migrations.push(plan.single_step_migration()?.clone());
    }
    Ok(migrations)
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
    /// Any other explicit scheme (`redis://`, …) — unsupported by this binary.
    /// Only Postgres + SQLite are supported.
    Unsupported,
}

/// Classify a `--database-url` into the [`Engine`] this CLI dispatches to, using
/// the SAME single-source grammar as `zeroship_core::db_url::is_sqlite_url` /
/// plugin-db's `backend_for_url` (no drift between the opener and the CLI).
///
/// - `postgres://` / `postgresql://` ⇒ [`Engine::Postgres`].
/// - `sqlite:` / `sqlite://` / `file:` / `:memory:` / a bare path ⇒
///   [`Engine::Sqlite`] with the extracted file path.
/// - an explicit but unrecognised scheme (e.g. `redis://`) ⇒ [`Engine::Unsupported`].
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

/// The operator's EXPLICIT engine choice (`--engine pg|sqlite`). Distinct from the
/// resolved [`Engine`] (which carries the SQLite app-file path): this is just the
/// kind, before a DSN is bound. It overrides DSN auto-detection and is checked for
/// contradiction against an unambiguous DSN scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// `--engine pg` — force the Postgres backend.
    Postgres,
    /// `--engine sqlite` — force the SQLite backend.
    Sqlite,
}

impl EngineKind {
    /// The operator-facing name, for error messages.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "pg",
            Self::Sqlite => "sqlite",
        }
    }
}

/// The engine a DSN's UNAMBIGUOUS scheme implies, if any. `postgres://` /
/// `postgresql://` / a libpq keyword DSN ⇒ Postgres; `sqlite:` / `sqlite://` /
/// `file:` / `:memory:` ⇒ SQLite. A BARE filesystem path is AMBIGUOUS (it has no
/// scheme — it defaults to SQLite under auto-detect, but `--engine` may legitimately
/// resolve it either way), so this returns `None` for it. An unknown explicit
/// scheme also returns `None` (it has no supported engine to contradict). This is
/// the predicate the conflict check uses: a contradiction only exists when the DSN
/// UNAMBIGUOUSLY names a different engine than `--engine`.
fn dsn_scheme_engine(url: &str) -> Option<EngineKind> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("postgres://")
        || lower.starts_with("postgresql://")
        || is_libpq_keyword_dsn(trimmed)
    {
        return Some(EngineKind::Postgres);
    }
    if lower == ":memory:"
        || lower.starts_with("sqlite://")
        || lower.starts_with("sqlite:")
        || lower.starts_with("file:")
    {
        return Some(EngineKind::Sqlite);
    }
    // A bare path (ambiguous) or an unknown scheme (no supported engine): the
    // explicit `--engine` has nothing to contradict.
    None
}

/// Resolve the [`Engine`] to dispatch onto, honouring an explicit `--engine`
/// override and DSN auto-detection.
///
/// - `override_kind = None` ⇒ pure DSN auto-detect ([`classify_engine`]) — the
///   default; existing invocations are byte-identical.
/// - `override_kind = Some(k)` ⇒ `k` wins. If the DSN's scheme UNAMBIGUOUSLY names
///   a *different* engine ([`dsn_scheme_engine`]), that is a hard
///   [`RunError::EngineConflict`] (we never silently trust one side). For an
///   AMBIGUOUS bare-path DSN the override resolves it deterministically: `pg` ⇒
///   [`Engine::Postgres`], `sqlite` ⇒ the hardened SQLite backend on that path.
///
/// # Errors
/// [`RunError::EngineConflict`] when the override contradicts an unambiguous DSN
/// scheme.
pub fn resolve_engine(database_url: &str, override_kind: Option<EngineKind>) -> Result<Engine, RunError> {
    let Some(kind) = override_kind else {
        return Ok(classify_engine(database_url));
    };
    if let Some(dsn_kind) = dsn_scheme_engine(database_url) {
        if dsn_kind != kind {
            return Err(RunError::EngineConflict {
                requested: kind.as_str(),
                dsn: dsn_kind.as_str(),
            });
        }
    } else if classify_engine(database_url) == Engine::Unsupported {
        // The DSN has no engine the override could resolve: an UNSUPPORTED explicit
        // scheme (`redis://`, `mysql://h/db`, …). `dsn_scheme_engine` returns `None`
        // for BOTH this and an ambiguous bare path, but only a bare path is
        // override-resolvable (it classifies as SQLite, not Unsupported). An
        // unsupported explicit scheme must still REFUSE under an override — never be
        // coerced into a file literally named `redis://h` (sqlite) or a doomed libpq
        // attempt (pg). With no override, `classify_engine` already refuses; the
        // override must not make it worse.
        return Err(RunError::UnsupportedEngine);
    }
    Ok(match kind {
        EngineKind::Postgres => Engine::Postgres,
        // For an ambiguous bare path the DSN has no scheme; `sqlite_file_path`
        // returns it verbatim. For a `sqlite:`/`file:`/`:memory:` DSN it strips the
        // scheme the same way `classify_engine` would.
        EngineKind::Sqlite => Engine::Sqlite(sqlite_file_path(database_url)),
    })
}

impl RunConfig {
    /// The [`Engine`] this config dispatches onto: [`resolve_engine`] applied to the
    /// DSN + the `engine_override`. The single seam every runner command goes
    /// through, so `--engine` is honoured uniformly.
    ///
    /// # Errors
    /// [`RunError::EngineConflict`] when `engine_override` contradicts the DSN.
    pub fn resolve_engine(&self) -> Result<Engine, RunError> {
        resolve_engine(&self.database_url, self.engine_override)
    }
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
    match cfg.resolve_engine()? {
        Engine::Postgres => run_migrate_pg(cfg).await,
        Engine::Sqlite(app) => run_migrate_sqlite(cfg, &app).await,
        Engine::Unsupported => Err(RunError::UnsupportedEngine),
    }
}

/// The SQLite leg of `migrate`: plan with the SQLite guard, gate destructive, then
/// apply through the hardened [`SqliteBackend`] via the SAME generic engine path.
async fn run_migrate_sqlite(cfg: &RunConfig, app: &Path) -> Result<RunReport, RunError> {
    let migrations = load_dir_flat(&cfg.dir)?;
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
    let migrations = load_dir_flat(&cfg.dir)?;
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
    let migrations = load_dir_flat(&cfg.dir)?;
    let st = match cfg.resolve_engine()? {
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
/// Engine-agnostic (dbmate parity): Postgres dry-runs on a throwaway shadow
/// DATABASE (CREATE/DROP DATABASE on a clone); SQLite dry-runs on a throwaway
/// SQLite file (a fresh `<file>.shadow` the CLI creates, replays the set onto, and
/// deletes) — no admin CREATEDB needed. Both engines then run the SAME checksum
/// drift check + destructive advisories, and emit the same [`ValidateReport`]
/// shape. An unsupported URL is an honest refusal.
///
/// # Errors
/// [`RunError`] on an unsupported engine / load / connect / the shadow harness / a
/// drift read failure.
pub async fn run_validate(cfg: &RunConfig) -> Result<RunReport, RunError> {
    match cfg.resolve_engine()? {
        Engine::Postgres => run_validate_pg(cfg).await,
        Engine::Sqlite(app) => run_validate_sqlite(cfg, &app).await,
        Engine::Unsupported => Err(RunError::UnsupportedEngine),
    }
}

/// Extract the per-migration `(version, advisories)` from a plan + the plan's
/// destructiveness — the ENGINE-AGNOSTIC half of `validate` both legs share (no
/// DB touched). Factored so the PG + SQLite legs report the identical advisory /
/// destructive surface rather than duplicating the extraction.
fn validate_plan_signals(
    plan: &MigrationPlan,
) -> (Vec<(String, Vec<Advisory>)>, bool) {
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
    (advisories, plan.destructive || plan.requires_approval)
}

/// The Postgres leg of `validate` — the existing path, byte-identical.
async fn run_validate_pg(cfg: &RunConfig) -> Result<RunReport, RunError> {
    let migrations = load_dir_flat(&cfg.dir)?;
    let conn = connect(&cfg.database_url).await?;
    let exec_cfg = build_exec_cfg(cfg);
    let guard_cfg = build_guard_cfg(cfg);
    let engine = MigrationEngine::new();

    // The plan tells us destructiveness + advisories WITHOUT touching the DB.
    let plan = engine.plan(&migrations, &guard_cfg);
    let (advisories, destructive) = validate_plan_signals(&plan);

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
        destructive,
        advisories,
    })))
}

/// The SQLite leg of `validate` — the dbmate-parity peer of [`run_validate_pg`].
///
/// SQLite has no admin `CREATEDB` and the backend deliberately exposes no
/// `ShadowDryRun` capability (`shadow()` is `None`), so we build the shadow
/// OURSELVES: a fresh throwaway SQLite file alongside the app file (a unique
/// `<app>.<uuid>.shadow` plus its sibling journal), onto which we REPLAY the loaded
/// set through the SAME generic engine `apply` the real `migrate` uses. The shadow
/// file and journal are removed on exit (every path, success and error). The
/// throwaway clone is created, applied to, and torn down WITHOUT touching the real
/// DB's schema. We then run the SAME checksum-drift check and destructive
/// advisories the PG leg runs (shared `validate_plan_signals` and the
/// backend-generic `check_checksum_drift`), emitting the same [`ValidateReport`]
/// shape.
async fn run_validate_sqlite(cfg: &RunConfig, app: &Path) -> Result<RunReport, RunError> {
    let migrations = load_dir_flat(&cfg.dir)?;
    let exec_cfg = sqlite_exec_cfg(cfg);
    let guard_cfg = sqlite_guard_cfg(cfg);
    let engine = MigrationEngine::new();

    // The plan: destructiveness + advisories WITHOUT touching any DB (same as PG).
    let plan = engine.plan(&migrations, &guard_cfg);
    let (advisories, destructive) = validate_plan_signals(&plan);

    // --- shadow dry-run on a THROWAWAY SQLite clone -------------------------
    // A fresh shadow file (+ its sibling journal) the CLI owns, applies the set
    // onto, and deletes — the SQLite peer of the PG throwaway shadow DATABASE. The
    // real app file is never opened for the dry-run.
    let shadow_app = sqlite_shadow_path(app);
    let dry_run = sqlite_shadow_dry_run(&shadow_app, &migrations, &exec_cfg, &guard_cfg).await;
    // Best-effort cleanup of the throwaway shadow + its journal (never fails the
    // command; a leaked dev temp file is harmless and `:memory:` has no file).
    cleanup_sqlite_shadow(&shadow_app);
    let dry_run = dry_run?;

    // --- checksum drift against the REAL journal (read-only) ----------------
    // Bootstrap the journal first (idempotent), exactly as `status` does, then run
    // the backend-generic drift comparison — identical rules to the PG leg.
    let backend = open_sqlite_backend(app)?;
    backend.ensure_journal(&exec_cfg).await?;
    let drift = backend.check_checksum_drift(&exec_cfg, &migrations).await?;

    Ok(RunReport::Validate(Box::new(ValidateReport {
        dry_run,
        drift,
        destructive,
        advisories,
    })))
}

/// The throwaway SQLite shadow app-file path for `validate`: a sibling
/// `<file>.<uuid>.shadow` (engine-constructed, never creator input). The per-call
/// UUID v4 suffix makes the path UNIQUE per invocation so two concurrent `validate`
/// runs on the SAME app file never collide on a fixed `<file>.shadow` (one run's
/// teardown must never delete another run's in-flight shadow). For `:memory:` the
/// shadow is its own `:memory:` (a private throwaway DB by nature — each open is
/// fresh, so no on-disk name to disambiguate).
fn sqlite_shadow_path(app: &Path) -> PathBuf {
    if app.as_os_str() == ":memory:" {
        return PathBuf::from(":memory:");
    }
    let mut s = app.to_path_buf().into_os_string();
    // UUID v4 (the crate's existing uuid facility) — a fresh random id per call, so
    // distinct invocations get distinct shadow files even on the same app path.
    s.push(format!(".{}.shadow", uuid::Uuid::new_v4().simple()));
    PathBuf::from(s)
}

/// Replay the loaded set onto a FRESH throwaway SQLite shadow + report each
/// migration's outcome as a [`DryRunReport`] — the SQLite-built peer of the PG
/// shadow harness. The shadow path is UNIQUE per invocation ([`sqlite_shadow_path`]
/// stamps a UUID), so the clone always starts empty without a pre-cleanup blow-away
/// (which would race a concurrent run's in-flight shadow). The set is applied
/// through the SAME generic engine `apply` the real `migrate` uses (with the plan's
/// `Approval` so a destructive migration still dry-runs on the throwaway clone), so
/// the dry-run is a FAITHFUL replay, not a parse-only check.
///
/// On a mid-batch failure the report mirrors the PG shadow leg's attribution: the
/// migrations that COMMITTED before the failure (read back from the shadow journal)
/// are recorded `applied_ok: true`, and the failure is attributed to the migration
/// that ACTUALLY failed — the first plan item NOT in the shadow journal — not
/// blanket-blamed on `plan.items.first()`. (A SQLite `up` failure surfaces as the
/// dialect-neutral `ApplyError::Backend(_)` which carries no version, so the
/// offending version is recovered from the committed-prefix rather than the typed
/// error — the journal is the ground truth for what landed on the shadow.)
async fn sqlite_shadow_dry_run(
    shadow_app: &Path,
    migrations: &[crate::migration::Migration],
    exec_cfg: &ExecutorConfig,
    guard_cfg: &super::GuardConfig,
) -> Result<crate::shadow::DryRunReport, RunError> {
    // No pre-cleanup blow-away: the shadow path is unique per invocation, so the
    // file does not pre-exist (and a fixed-name blow-away would delete a concurrent
    // run's in-flight shadow). Open the fresh clone directly.
    let shadow_journal = sqlite_journal_path(shadow_app);
    let backend =
        SqliteBackend::open(shadow_app, &shadow_journal).map_err(|e| RunError::Sqlite(e.to_string()))?;

    let engine = MigrationEngine::new();
    let plan = engine.plan(migrations, guard_cfg);
    // The throwaway clone is ours to mutate freely; replay the WHOLE set (a
    // destructive migration is approved here — it only touches the shadow, never
    // the real DB), recording per-migration success/failure into the report.
    let mut per_migration = Vec::new();
    let mut ok = true;
    match engine
        .apply(&plan, Approval::Approved, &backend, exec_cfg, "platform-validate")
        .await
    {
        Ok(outcome) => {
            for v in outcome.applied.iter().chain(outcome.recovered.iter()) {
                per_migration.push(crate::shadow::MigrationResult {
                    version: v.clone(),
                    applied_ok: true,
                    error: None,
                });
            }
        }
        Err(e) => {
            // The batch aborted. Mirror the PG shadow leg: record the committed
            // PREFIX as applied, and attribute the failure to the migration that
            // ACTUALLY failed (the first plan item not committed), rather than
            // blanket-blaming `plan.items.first()`. The shadow journal is the ground
            // truth — every migration whose atomic apply COMMITTED before the abort
            // has a `completed` event there (the failed one rolled back, so it does
            // not). This recovers the true offending version even when the SQLite
            // `up` failure surfaces as the version-less `ApplyError::Backend(_)`.
            ok = false;
            let committed = shadow_committed_versions(&backend, exec_cfg).await;
            for p in &plan.items {
                let version = p.migration.version.as_str().to_string();
                if committed.contains(&version) {
                    // Applied before the failure → success-prefix entry.
                    per_migration.push(crate::shadow::MigrationResult {
                        version,
                        applied_ok: true,
                        error: None,
                    });
                } else {
                    // The first not-committed plan item is the one that failed; the
                    // engine aborts the batch on the first failure, so nothing after
                    // it ran. Attribute the error here and stop.
                    per_migration.push(crate::shadow::MigrationResult {
                        version,
                        applied_ok: false,
                        error: Some(e.to_string()),
                    });
                    break;
                }
            }
            // Defensive: if every plan item somehow committed yet apply errored
            // (e.g. a post-loop engine error), still surface the failure with a
            // batch-level entry so `ok=false` is never silently empty.
            if per_migration.iter().all(|r| r.applied_ok) {
                per_migration.push(crate::shadow::MigrationResult {
                    version: plan
                        .items
                        .first()
                        .map_or_else(String::new, |p| p.migration.version.as_str().to_string()),
                    applied_ok: false,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    Ok(crate::shadow::DryRunReport {
        ok,
        per_migration,
        resulting_drift: None,
        advisories: Vec::new(),
        teardown_ok: true,
        teardown_error: None,
    })
}

/// Read the shadow journal's net-applied (`completed`) versions — the set of
/// migrations whose atomic apply COMMITTED on the throwaway clone. Used by
/// [`sqlite_shadow_dry_run`] to reconstruct the success prefix + locate the true
/// offending migration after a mid-batch failure. Best-effort: a journal read
/// failure yields an empty set (the failure is still surfaced via the apply error).
async fn shadow_committed_versions(
    backend: &SqliteBackend,
    exec_cfg: &ExecutorConfig,
) -> std::collections::HashSet<String> {
    use crate::journal::Phase;
    backend.applied(exec_cfg).await.map_or_else(
        |_| std::collections::HashSet::new(),
        |rows| {
            rows.into_iter()
                .filter(|e| e.phase == Phase::Completed)
                .map(|e| e.version)
                .collect()
        },
    )
}

/// Best-effort removal of a throwaway SQLite shadow file + its sibling journal (and
/// the WAL/SHM side files if any). Never errors — a leaked dev temp file is
/// harmless, and `:memory:` has no on-disk file to remove.
fn cleanup_sqlite_shadow(shadow_app: &Path) {
    if shadow_app.as_os_str() == ":memory:" {
        return;
    }
    let journal = sqlite_journal_path(shadow_app);
    for base in [shadow_app.to_path_buf(), journal] {
        let _ = std::fs::remove_file(&base);
        // Rollback-journal / WAL side files SQLite may leave alongside the DB.
        for suffix in ["-journal", "-wal", "-shm"] {
            let mut s = base.clone().into_os_string();
            s.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(s));
        }
    }
}

/// `dump` (SQLite leg) — serialize the LIVE `main` schema as a deterministic
/// CREATE-statement script, derived from `sqlite_master` through the hardened
/// [`SqliteBackend`] (NOT a `sqlite3` shell-out). The engine-agnostic `dump`
/// parity peer of the PG `pg_dump --schema-only` leg; the bin appends the SAME
/// applied-versions trailer to both.
///
/// The `_mig` journal (a separate attached DB) and `sqlite_*` internals never
/// appear in the output (§2.5.2) — the `main`-scoped read cannot see them.
///
/// # Errors
/// [`RunError::Sqlite`] on a `sqlite_master` read / backend-open failure.
pub async fn dump_schema_sqlite(app: &Path) -> Result<String, RunError> {
    let backend = open_sqlite_backend(app)?;
    backend
        .dump_schema_sqlite()
        .await
        .map_err(|e| RunError::Sqlite(e.to_string()))
}

/// Net-applied migrations as [`TrailerEntry`]s (version + checksum + name) sourced
/// from the JOURNAL — what the bin's `dump` writes into the schema-file trailer
/// (M1+M2). Because the checksum + name come from the journal (not re-derived from
/// `--dir`), the dump→load round-trip is faithful even if a migration file is
/// later edited or deleted. Dispatches by engine.
///
/// # Errors
/// [`RunError`] on an unsupported engine / connect / a journal read failure.
pub async fn run_dump_trailer(cfg: &RunConfig) -> Result<Vec<TrailerEntry>, RunError> {
    match cfg.resolve_engine()? {
        Engine::Postgres => {
            let conn = connect(&cfg.database_url).await?;
            let exec_cfg = build_exec_cfg(cfg);
            // Ensure the journal exists (same idempotent bootstrap `status` does) so a
            // fresh DB yields an empty trailer rather than erroring on a missing table.
            crate::journal::ensure_journal(&conn, &exec_cfg).await?;
            pg_net_applied_trailer(&conn, &exec_cfg).await
        }
        Engine::Sqlite(app) => {
            let backend = open_sqlite_backend(&app)?;
            let exec_cfg = sqlite_exec_cfg(cfg);
            backend.ensure_journal(&exec_cfg).await?;
            let rows = backend
                .net_applied_trailer_sqlite()
                .await
                .map_err(|e| RunError::Sqlite(e.to_string()))?;
            Ok(rows
                .into_iter()
                .map(|(version, name, checksum)| TrailerEntry {
                    version,
                    checksum,
                    name,
                })
                .collect())
        }
        Engine::Unsupported => Err(RunError::UnsupportedEngine),
    }
}

/// PG net-applied `(version, checksum, name)` for the dump trailer — per version the
/// LATEST event must be `applied`; its name/checksum are read from that completed
/// event. Mirrors [`crate::journal::applied`]'s DISTINCT-ON-latest logic, additionally
/// carrying `name` (which `AppliedEntry` does not). Ordered by version.
async fn pg_net_applied_trailer(
    conn: &compio_postgres::Client,
    exec_cfg: &ExecutorConfig,
) -> Result<Vec<TrailerEntry>, RunError> {
    let meta = format!("\"{}\"", exec_cfg.pg.meta_schema.replace('"', "\"\""));
    let rows = conn
        .query(
            &format!(
                "WITH latest AS (
                     SELECT DISTINCT ON (version) version, name, checksum, event_kind
                       FROM {meta}.schema_migrations
                      ORDER BY version, event_seq DESC
                 )
                 SELECT version, name, checksum
                   FROM latest
                  WHERE event_kind = '{applied}'
                  ORDER BY version COLLATE \"C\"",
                applied = crate::journal::EventKind::Applied.as_str()
            ),
            &[],
        )
        .await
        .map_err(crate::journal::JournalError::Db)?;
    Ok(rows
        .iter()
        .map(|r| TrailerEntry {
            version: r.get("version"),
            name: r.get("name"),
            checksum: r.get("checksum"),
        })
        .collect())
}

/// The marker line that separates a dump's DDL body from its applied-versions
/// trailer (dbmate `db:dump`/`db:load` round-trip). The bin's `dump` writes it
/// anchored to its own line — `\n{TRAILER_MARKER}\n` — then one trailer line per
/// applied migration; [`parse_schema_dump`] splits on exactly this line-anchored
/// marker so `load` faithfully reconstructs the journal from what `dump` wrote.
pub const SCHEMA_TRAILER_MARKER: &str = "-- zeroship-migrate schema_migrations";

/// The exact line-anchored form of [`SCHEMA_TRAILER_MARKER`] the bin's `dump`
/// emits (`\n` + marker + `\n`). [`parse_schema_dump`] splits on THIS — a marker
/// substring appearing inside a DDL comment body (not on its own line, or not
/// followed by a newline) does not trip the split.
const SCHEMA_TRAILER_ANCHOR: &str = "\n-- zeroship-migrate schema_migrations\n";

/// The per-migration trailer-line prefix (`--   <version>\t<checksum>\t<name>`).
/// The three spaces are the dump writer's exact format; the fields are tab-
/// delimited so a name containing spaces round-trips unambiguously.
const SCHEMA_TRAILER_VERSION_PREFIX: &str = "--   ";

/// One applied migration reconstructed from a dump trailer line: the version plus
/// the checksum + name sourced from the JOURNAL at dump time. `load` reconstructs
/// the journal from these alone — it does NOT re-source checksums from `--dir`, so
/// an edited/missing migration file cannot make `status`/drift lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailerEntry {
    /// The migration version (`<14-digit>` / `mig_…`).
    pub version: String,
    /// The checksum recorded in the dump trailer (sourced from the journal).
    pub checksum: String,
    /// The migration name recorded in the dump trailer (sourced from the journal).
    pub name: String,
}

/// Split a dumped `schema.sql` into (DDL body, applied migrations), the EXACT
/// inverse of what the bin's `dump` writes (so dump↔load round-trips faithfully).
///
/// The body is everything BEFORE the LINE-ANCHORED [`SCHEMA_TRAILER_ANCHOR`]; the
/// migrations are parsed from the tab-delimited `--   <version>\t<checksum>\t<name>`
/// lines after it. The marker is matched anchored to a line boundary and the LAST
/// occurrence is taken (the trailer is always appended at the very end), so a DDL
/// body that legitimately contains the marker text in a comment still parses
/// correctly. A dump with no trailer (e.g. a hand-written schema) yields the whole
/// content as the body and an empty migration list.
///
/// # Errors
/// [`RunError::LoadCmd`] on a malformed/truncated trailer line (a non-empty trailer
/// line that is not `--   <version>\t<checksum>\t<name>`) — a corrupt dump must
/// fail cleanly, never silently drop a migration's checksum/name.
pub fn parse_schema_dump(content: &str) -> Result<(String, Vec<TrailerEntry>), RunError> {
    let Some(idx) = content.rfind(SCHEMA_TRAILER_ANCHOR) else {
        return Ok((content.to_string(), Vec::new()));
    };
    // The body keeps the leading `\n` of the anchor (it was the body's own trailing
    // newline before the marker line); the trailer is everything after the marker
    // line's own newline.
    let body = format!("{}\n", &content[..idx]);
    let trailer = &content[idx + SCHEMA_TRAILER_ANCHOR.len()..];
    let mut entries = Vec::new();
    for raw in trailer.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix(SCHEMA_TRAILER_VERSION_PREFIX) else {
            // A non-empty trailer line that is not a version line is a corrupt dump.
            return Err(RunError::LoadCmd(format!(
                "malformed schema dump trailer line (expected `{SCHEMA_TRAILER_VERSION_PREFIX}<version>\\t<checksum>\\t<name>`): {line:?}"
            )));
        };
        let mut fields = rest.splitn(3, '\t');
        let version = fields.next().unwrap_or("").trim();
        let checksum = fields.next();
        let name = fields.next();
        match (version, checksum, name) {
            (v, Some(c), Some(n)) if !v.is_empty() => entries.push(TrailerEntry {
                version: v.to_string(),
                checksum: c.to_string(),
                name: n.to_string(),
            }),
            _ => {
                return Err(RunError::LoadCmd(format!(
                    "malformed schema dump trailer line (expected `{SCHEMA_TRAILER_VERSION_PREFIX}<version>\\t<checksum>\\t<name>`): {line:?}"
                )));
            }
        }
    }
    Ok((body, entries))
}

/// The pg_dump SESSION-PREAMBLE settings — pure dump-session cosmetics emitted as a
/// `SET <key> = …;` (or `SELECT pg_catalog.set_config('search_path', …)`) block at
/// the top of every `pg_dump --schema-only`. They configure the DUMPING session,
/// not the schema, and the dumped DDL is fully schema-qualified (`public.…`) so it
/// does not depend on them. Stripping them on restore makes `load` robust across
/// pg_dump↔server VERSION SKEW (e.g. a pg_dump ≥17 `SET transaction_timeout = 0;`
/// that an older server rejects as an unrecognised GUC) — dbmate sidesteps this by
/// piping to `psql`, but we restore over the SQL protocol where one rejected GUC
/// would abort the whole batch.
const PG_DUMP_SESSION_SETTINGS: &[&str] = &[
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "transaction_timeout",
    "client_encoding",
    "standard_conforming_strings",
    "check_function_bodies",
    "xmloption",
    "client_min_messages",
    "row_security",
    "default_tablespace",
    "default_table_access_method",
    "escape_string_warning",
    "search_path",
];

/// `true` iff `line` is a pg_dump session-preamble setting line (`SET <known> = …;`
/// or `SELECT pg_catalog.set_config('search_path', …);`) safe to drop on restore.
///
/// L1 over-strip guard: a line is only recognised when it is a SINGLE, COMPLETE
/// statement — it must END IN `;`. pg_dump emits each preamble `SET`/`set_config`
/// on exactly one self-terminated line, so this never misses a real preamble line;
/// but it refuses to drop a line that merely STARTS with `SET …` and continues
/// onto the next line (a hypothetical multi-line statement whose first line happens
/// to begin with an allowlisted keyword), which would otherwise corrupt the body.
fn is_pg_dump_session_setting(line: &str) -> bool {
    let t = line.trim();
    // The line-oriented stripping in `strip_psql_meta_commands` only sees ONE line
    // at a time; a preamble setting is always a single self-terminated statement, so
    // require the closing `;` before treating it as droppable.
    if !t.ends_with(';') {
        return false;
    }
    if let Some(rest) = t.strip_prefix("SET ").or_else(|| t.strip_prefix("set ")) {
        let key = rest
            .split(|c: char| c == '=' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        return PG_DUMP_SESSION_SETTINGS.contains(&key.as_str());
    }
    // The `SELECT pg_catalog.set_config('search_path', '', false);` form.
    let lower = t.to_ascii_lowercase();
    lower.starts_with("select pg_catalog.set_config('search_path'")
        || lower.starts_with("select set_config('search_path'")
}

/// Sanitize a `pg_dump --schema-only` body for restore over the raw SQL protocol
/// (we do not shell `psql`). Strips two classes of NON-schema noise, line-oriented
/// (a multi-line SQL statement is preserved verbatim):
/// 1. `psql` META-COMMANDS — backslash lines like the `\restrict`/`\unrestrict`
///    directives pg_dump ≥17 emits (psql client directives, not SQL).
/// 2. The pg_dump SESSION-PREAMBLE `SET`/`set_config` settings (see
///    [`PG_DUMP_SESSION_SETTINGS`]) — dump-session cosmetics that would otherwise
///    abort the restore batch under pg_dump↔server version skew.
///
/// Everything else (the real schema DDL) is the verbatim dump.
///
/// NOTE: the stripping is LINE-ORIENTED and assumes pg_dump's single-line preamble
/// — each `\`-meta-command and each `SET`/`set_config` setting occupies exactly one
/// self-terminated line. [`is_pg_dump_session_setting`] additionally requires the
/// trailing `;` (L1) so a multi-line statement whose first line happens to begin
/// with an allowlisted keyword is never partially stripped.
fn strip_psql_meta_commands(ddl: &str) -> String {
    let mut out = String::with_capacity(ddl.len());
    for line in ddl.lines() {
        if line.trim_start().starts_with('\\') || is_pg_dump_session_setting(line) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// What [`run_load`] did — printable by the bin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOutcome {
    /// The versions journaled (reconstructed from the dump trailer), in dump order.
    pub versions: Vec<String>,
}

/// `load` (a.k.a. `db:setup`) — RESTORE a dumped `schema.sql` onto a FRESH database
/// and reconstruct the journal from the dump's applied-versions trailer, WITHOUT
/// replaying every migration (dbmate `db:load`).
///
/// Two halves, both engine-dispatched:
/// 1. **Execute the DDL** body (everything before [`SCHEMA_TRAILER_MARKER`]) to
///    recreate the schema — a restore of an operator-/engine-generated dump (the
///    Trusted CLI posture pipes it straight at the engine, exactly as dbmate pipes
///    `schema.sql` to `psql`).
/// 2. **Reconstruct the journal** — record each trailer version as a
///    `baseline`-kind `completed` event (recorded-not-run) via the SAME immutable
///    journal path [`baseline`](crate::baseline) uses, so the versions end up
///    net-applied and a subsequent `migrate` runs only the genuinely-pending ones.
///    Each version's name + checksum is sourced from its migration file in `--dir`
///    (so the recorded checksum matches the file and drift stays clean).
///
/// **First-entry-only** (mirrors baseline): refuses if the target DB already records
/// any net-applied migration — `load` bootstraps a FRESH DB and must never clobber a
/// DB the engine already manages. `content` is the schema-file text the bin read.
///
/// # Errors
/// [`RunError`] on an unsupported engine / connect / a DDL failure / an
/// already-managed DB / a journal-write failure.
pub async fn run_load(cfg: &RunConfig, content: &str) -> Result<RunReport, RunError> {
    // C1: `load` restores an operator-/engine-generated dump by piping its DDL
    // STRAIGHT at the engine (admin connection, NO guard().check, NO migrator
    // role) — exactly as dbmate pipes schema.sql to `psql`. That posture is sound
    // ONLY under the Trusted profile (the operator owns the DB). Under
    // `confined`/`platform` it would execute raw DDL as admin and bypass the entire
    // confinement model, so refuse BEFORE any DB connection or DDL.
    if cfg.profile != RunProfile::Trusted {
        return Err(RunError::LoadCmd(
            "load restores a dump as admin without the guard/migrator role; \
             it is only available under --profile trusted"
                .to_string(),
        ));
    }

    // M1+M2: the trailer is self-contained — each entry carries the version +
    // checksum + name sourced from the JOURNAL at dump time. `load` reconstructs
    // the journal from these ALONE (NOT from `--dir`), so an edited/missing
    // migration file cannot make `status`/drift lie or trip orphan-drift.
    let (ddl, entries) = parse_schema_dump(content)?;

    match cfg.resolve_engine()? {
        Engine::Postgres => run_load_pg(cfg, &ddl, &entries).await,
        Engine::Sqlite(app) => run_load_sqlite(cfg, &app, &ddl, &entries).await,
        Engine::Unsupported => Err(RunError::UnsupportedEngine),
    }
}

/// The Postgres leg of `load`: restore the DDL via the admin connection, then
/// record each trailer version as a baseline `completed` event under the project
/// advisory lock (the SAME serialization + immutable journal path `migrate` uses).
async fn run_load_pg(
    cfg: &RunConfig,
    ddl: &str,
    entries: &[TrailerEntry],
) -> Result<RunReport, RunError> {
    let conn = connect(&cfg.database_url).await?;
    let exec_cfg = build_exec_cfg(cfg);

    // Serialize against all migration activity (design §2.3), exactly like apply /
    // baseline. Held for the whole load; released on every exit.
    conn.execute(
        "SELECT pg_advisory_lock(hashtext($1)::bigint)",
        &[&cfg.project_id],
    )
    .await
    .map_err(crate::journal::JournalError::Db)?;
    let result = run_load_pg_locked(&conn, &exec_cfg, ddl, entries).await;
    let unlock = conn
        .execute(
            "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
            &[&cfg.project_id],
        )
        .await;
    let outcome = result?;
    unlock.map_err(crate::journal::JournalError::Db)?;
    Ok(RunReport::Load(outcome))
}

/// The PG `load` body, run while holding the project advisory lock: first-entry
/// guard → restore DDL → journal the trailer versions.
async fn run_load_pg_locked(
    conn: &compio_postgres::Client,
    exec_cfg: &ExecutorConfig,
    ddl: &str,
    entries: &[TrailerEntry],
) -> Result<LoadOutcome, RunError> {
    // H2: first-entry guard, BEFORE any DDL. `load` bootstraps a FRESH DB and must
    // never clobber one the engine already manages OR one that already carries user
    // objects. Two checks:
    //   (a) the EXISTING journal records no net-applied migration (probed WITHOUT
    //       creating the table — an `ensure_journal` here would pre-create
    //       `<meta>.schema_migrations`, which then collides with the dump's own
    //       `CREATE TABLE … schema_migrations` on restore); and
    //   (b) the target schema holds no user tables (a dump-less hand-populated DB
    //       has no journal but is NOT empty — restoring over it would error mid-
    //       batch on the first `CREATE TABLE` collision, leaving a half state).
    let net = load_pg_net_applied(conn, exec_cfg).await?;
    if !net.is_empty() {
        return Err(RunError::LoadCmd(format!(
            "cannot load: the journal already records {} net-applied migration(s); \
             `load` targets a fresh/empty database (a DB the engine already manages \
             cannot be loaded over)",
            net.len()
        )));
    }
    let user_tables = load_pg_user_tables(conn, exec_cfg).await?;
    if user_tables > 0 {
        return Err(RunError::LoadCmd(format!(
            "cannot load: the target schema {:?} already has {} user table(s); \
             `load` targets a fresh/empty database",
            exec_cfg.project_schema, user_tables
        )));
    }

    // 1. Restore the dump DDL inside ONE explicit transaction (H1) so a failure
    //    mid-dump rolls back to the pre-load state rather than leaving a half-
    //    restored DB. A `pg_dump --schema-only` of zeroship schemas is txn-safe (no
    //    CREATE DATABASE / CREATE INDEX CONCURRENTLY / etc. in a schema-only dump).
    //    The dump RECREATES the journal objects too (they live in the dumped
    //    schema), so we must NOT pre-create them. `strip_psql_meta_commands` drops
    //    the psql backslash directives + the pg_dump session-`SET` preamble;
    //    everything else is verbatim.
    let sql = strip_psql_meta_commands(ddl);
    if !sql.trim().is_empty() {
        conn.batch_execute(&format!("BEGIN;\n{sql}\nCOMMIT;"))
            .await
            .map_err(|e| RunError::LoadCmd(format!("restore dump DDL: {e}")))?;
    }

    // 1b. Ensure the journal exists (idempotent `CREATE … IF NOT EXISTS`): a dump
    //     that did NOT carry the journal objects (e.g. a hand-written schema, or a
    //     platform meta-schema not in the dumped set) still needs them to record into.
    crate::journal::ensure_journal(conn, exec_cfg).await?;

    // 2. Journal each trailer entry as a baseline `completed` event (recorded-not-
    //    run), via the SAME immutable `record_baseline` path baseline uses. The
    //    version + checksum + name come from the dump trailer (M1+M2), NOT from
    //    `--dir`, so the journaled checksum is the dump's own → drift stays clean.
    for e in entries {
        crate::journal::record_baseline(
            conn,
            exec_cfg,
            crate::journal::BaselineRecord {
                version: &e.version,
                name: &e.name,
                checksum: &e.checksum,
                applied_by: "platform-load",
                kind: "baseline",
                supersedes: &[],
            },
        )
        .await?;
    }

    Ok(LoadOutcome {
        versions: entries.iter().map(|e| e.version.clone()).collect(),
    })
}

/// Count the user tables in the `load` target schema (the H2 schema-emptiness
/// probe — a DB with no journal but pre-existing user objects is NOT a fresh load
/// target). Counts base tables in the project schema only; the journal's own
/// `schema_migrations*` tables live there too but a fresh load target has none yet
/// (the journal-presence check `load_pg_net_applied` already covers the managed
/// case, so any table here on a journal-less DB is genuine user data).
async fn load_pg_user_tables(
    conn: &compio_postgres::Client,
    exec_cfg: &ExecutorConfig,
) -> Result<i64, RunError> {
    let rows = conn
        .query(
            "SELECT count(*)::bigint AS n
               FROM information_schema.tables
              WHERE table_schema = $1
                AND table_type = 'BASE TABLE'",
            &[&exec_cfg.project_schema],
        )
        .await
        .map_err(crate::journal::JournalError::Db)?;
    Ok(rows.first().map(|r| r.get::<_, i64>("n")).unwrap_or(0))
}

/// Probe the EXISTING PG journal's net-applied versions WITHOUT creating it (the
/// `load` first-entry guard). A `load` target with no journal table yet is FRESH;
/// pre-creating the table here would collide with the dump's own
/// `CREATE TABLE … schema_migrations` on restore. So we check the meta table's
/// existence first (via `to_regclass`, NULL when absent) and only read net state if
/// it exists.
async fn load_pg_net_applied(
    conn: &compio_postgres::Client,
    exec_cfg: &ExecutorConfig,
) -> Result<Vec<String>, RunError> {
    // Does `<meta>.schema_migrations` exist? `to_regclass` returns NULL if not.
    let qualified = format!("{}.schema_migrations", exec_cfg.pg.meta_schema);
    let row = conn
        .query("SELECT to_regclass($1) IS NOT NULL AS present", &[&qualified])
        .await
        .map_err(crate::journal::JournalError::Db)?;
    let present = row.first().map(|r| r.get::<_, bool>("present")).unwrap_or(false);
    if !present {
        return Ok(Vec::new());
    }
    let already = crate::journal::applied(conn, exec_cfg).await?;
    Ok(already
        .iter()
        .filter(|e| e.phase == crate::journal::Phase::Completed)
        .map(|e| e.version.as_str().to_string())
        .collect())
}

/// The SQLite leg of `load`: restore the DDL onto `main` then journal the trailer
/// versions through the hardened backend (first-entry-guarded).
async fn run_load_sqlite(
    cfg: &RunConfig,
    app: &Path,
    ddl: &str,
    entries: &[TrailerEntry],
) -> Result<RunReport, RunError> {
    let _ = cfg;
    let backend = open_sqlite_backend(app)?;
    // H2: refuse BEFORE any mutation if `main` already carries user objects OR the
    // journal already records net-applied migrations. The PRE-restore guard is
    // essential here: `restore_schema_sqlite` mutates `main`, and the old
    // first-entry check lived only INSIDE `record_loaded_versions_sqlite` (AFTER the
    // restore), so a non-fresh DB was already mutated by the time it was refused.
    backend
        .ensure_fresh_load_target_sqlite()
        .await
        .map_err(|e| RunError::Sqlite(e.to_string()))?;
    // 1. Restore the dump DDL onto `main`.
    backend
        .restore_schema_sqlite(ddl)
        .await
        .map_err(|e| RunError::Sqlite(e.to_string()))?;
    // 2. Journal the trailer entries (first-entry-guarded inside the backend too,
    //    as defense in depth). The version + checksum + name come from the dump
    //    trailer (M1+M2), NOT from `--dir`.
    let loaded: Vec<crate::backend_sqlite::LoadedVersion> = entries
        .iter()
        .map(|e| crate::backend_sqlite::LoadedVersion {
            version: e.version.clone(),
            name: e.name.clone(),
            checksum: e.checksum.clone(),
        })
        .collect();
    backend
        .record_loaded_versions_sqlite(&loaded, "platform-load")
        .await
        .map_err(|e| RunError::Sqlite(e.to_string()))?;
    Ok(RunReport::Load(LoadOutcome {
        versions: entries.iter().map(|e| e.version.clone()).collect(),
    }))
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

    match cfg.resolve_engine()? {
        Engine::Postgres => run_rollback_pg(cfg, target).await,
        Engine::Sqlite(app) => run_rollback_sqlite(cfg, &app, target).await,
        Engine::Unsupported => Err(RunError::UnsupportedEngine),
    }
}

/// The Postgres leg of `rollback` — the existing path, byte-identical.
async fn run_rollback_pg(cfg: &RunConfig, target: RollbackTarget) -> Result<RunReport, RunError> {
    let migrations = load_dir_flat(&cfg.dir)?;
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
    let migrations = load_dir_flat(&cfg.dir)?;
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
    let migrations = load_dir_flat(&cfg.dir)?;
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
    engine_override: Option<EngineKind>,
    timeout: Duration,
    interval: Duration,
) -> Result<(), RunError> {
    // Resolve the engine ONCE up front so an `--engine` conflict surfaces as a
    // clear error rather than failing every probe until the deadline.
    let engine = resolve_engine(database_url, engine_override)?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let last_err = match probe_once(&engine, database_url).await {
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
async fn probe_once(engine: &Engine, database_url: &str) -> Result<(), String> {
    match engine {
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
            open_sqlite_backend(app).map(|_| ()).map_err(|e| e.to_string())
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
            engine_override: None,
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
            engine_override: None,
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
        assert_eq!(classify_engine("redis://localhost:6379"), Engine::Unsupported);
    }

    #[test]
    fn resolve_engine_none_is_pure_dsn_autodetect() {
        // No override ⇒ byte-identical to classify_engine for every shape.
        for url in [
            "postgres://h/db",
            "host=h dbname=db",
            "sqlite:/tmp/x.db",
            "/bare/path.db",
            "redis://h",
        ] {
            assert_eq!(
                resolve_engine(url, None).unwrap_or(Engine::Unsupported),
                classify_engine(url),
                "no-override must match classify_engine for {url}"
            );
        }
    }

    #[test]
    fn resolve_engine_override_resolves_ambiguous_bare_path() {
        // A bare path is ambiguous; the override decides deterministically.
        assert_eq!(
            resolve_engine("/data/app.db", Some(EngineKind::Sqlite)).expect("ok"),
            Engine::Sqlite(PathBuf::from("/data/app.db"))
        );
        assert_eq!(
            resolve_engine("/data/app.db", Some(EngineKind::Postgres)).expect("ok"),
            Engine::Postgres
        );
    }

    #[test]
    fn resolve_engine_override_agreeing_with_dsn_is_ok() {
        assert_eq!(
            resolve_engine("sqlite:/tmp/x.db", Some(EngineKind::Sqlite)).expect("ok"),
            Engine::Sqlite(PathBuf::from("/tmp/x.db"))
        );
        assert_eq!(
            resolve_engine("postgres://h/db", Some(EngineKind::Postgres)).expect("ok"),
            Engine::Postgres
        );
    }

    #[test]
    fn resolve_engine_override_contradicting_unambiguous_dsn_is_conflict() {
        // --engine pg vs a sqlite: DSN.
        let err = resolve_engine("sqlite:/tmp/x.db", Some(EngineKind::Postgres)).unwrap_err();
        assert!(
            matches!(err, RunError::EngineConflict { requested: "pg", dsn: "sqlite" }),
            "expected an engine conflict, got {err:?}"
        );
        // --engine sqlite vs a postgres:// DSN.
        let err = resolve_engine("postgres://h/db", Some(EngineKind::Sqlite)).unwrap_err();
        assert!(
            matches!(err, RunError::EngineConflict { requested: "sqlite", dsn: "pg" }),
            "expected an engine conflict, got {err:?}"
        );
        // --engine sqlite vs a libpq keyword DSN (also unambiguously PG).
        let err = resolve_engine("host=h dbname=db", Some(EngineKind::Sqlite)).unwrap_err();
        assert!(matches!(err, RunError::EngineConflict { .. }));
    }

    /// MED-2 RED: an UNSUPPORTED explicit scheme (`redis://`, `mysql://…`) must NOT
    /// be coerced by `--engine` into a file/connect attempt. With no override,
    /// `classify_engine` already refuses (Unsupported); `--engine` must still refuse
    /// — it must never make the unsupported-scheme case WORSE. The url
    /// `redis://h` must NOT become `Engine::Sqlite(redis://h)` (a file literally
    /// named `redis://h`) under `--engine sqlite`, nor `Engine::Postgres` (a libpq
    /// attempt) under `--engine pg`.
    #[test]
    fn resolve_engine_override_does_not_coerce_unsupported_scheme() {
        // --engine sqlite + redis://h must refuse, NOT open a file named "redis://h".
        let err = resolve_engine("redis://h", Some(EngineKind::Sqlite)).unwrap_err();
        assert!(
            matches!(err, RunError::UnsupportedEngine | RunError::EngineConflict { .. }),
            "expected a clean refusal, got {err:?}"
        );
        // --engine pg + mysql://h/db must refuse, NOT hand mysql://h/db to libpq.
        let err = resolve_engine("mysql://h/db", Some(EngineKind::Postgres)).unwrap_err();
        assert!(
            matches!(err, RunError::UnsupportedEngine | RunError::EngineConflict { .. }),
            "expected a clean refusal, got {err:?}"
        );
        // --engine pg + redis://h must also refuse.
        let err = resolve_engine("redis://h", Some(EngineKind::Postgres)).unwrap_err();
        assert!(
            matches!(err, RunError::UnsupportedEngine | RunError::EngineConflict { .. }),
            "expected a clean refusal, got {err:?}"
        );
    }

    /// MED-2: an AMBIGUOUS bare path (no scheme) remains override-resolvable — the
    /// override must NOT be downgraded by the unsupported-scheme refusal. Guards the
    /// fix from over-refusing.
    #[test]
    fn resolve_engine_override_still_resolves_bare_path_after_fix() {
        assert_eq!(
            resolve_engine("/data/app.db", Some(EngineKind::Sqlite)).expect("ok"),
            Engine::Sqlite(PathBuf::from("/data/app.db"))
        );
        assert_eq!(
            resolve_engine("/data/app.db", Some(EngineKind::Postgres)).expect("ok"),
            Engine::Postgres
        );
    }

    #[test]
    fn dsn_scheme_engine_treats_bare_path_and_unknown_scheme_as_ambiguous() {
        assert_eq!(dsn_scheme_engine("/bare/path.db"), None);
        assert_eq!(dsn_scheme_engine("redis://h"), None);
        assert_eq!(dsn_scheme_engine("postgres://h/db"), Some(EngineKind::Postgres));
        assert_eq!(dsn_scheme_engine("sqlite:/x"), Some(EngineKind::Sqlite));
        assert_eq!(dsn_scheme_engine(":memory:"), Some(EngineKind::Sqlite));
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

    /// Fix #2 (shadow-file race): two `sqlite_shadow_path` calls for the SAME app
    /// file must yield DISTINCT paths so two concurrent `validate` runs never collide
    /// on a fixed `<file>.shadow` (and one run's teardown can never delete another's
    /// in-flight shadow). Pre-fix the path was deterministic (`<file>.shadow`) and
    /// these two would be equal — RED. Post-fix the per-call UUID suffix makes them
    /// distinct. Both remain siblings of the app file, prefixed by the app name and
    /// suffixed `.shadow`.
    #[test]
    fn sqlite_shadow_path_is_unique_per_invocation() {
        let app = PathBuf::from("/tmp/app.sqlite");
        let a = sqlite_shadow_path(&app);
        let b = sqlite_shadow_path(&app);
        assert_ne!(a, b, "two shadow paths for the same app must differ (no race)");
        for p in [&a, &b] {
            let s = p.to_string_lossy();
            assert!(
                s.starts_with("/tmp/app.sqlite.") && s.ends_with(".shadow"),
                "shadow path must be a `<app>.<uuid>.shadow` sibling: {s}"
            );
        }
        // `:memory:` is its own throwaway DB — no on-disk name to disambiguate.
        assert_eq!(
            sqlite_shadow_path(&PathBuf::from(":memory:")),
            PathBuf::from(":memory:")
        );
    }

    // ---- load: dump-trailer parse + restore-DDL sanitization ---------------

    /// `parse_schema_dump` is the EXACT inverse of what the bin's `dump` writes: the
    /// body is everything before the trailer marker; the entries are parsed from the
    /// tab-delimited `--   <version>\t<checksum>\t<name>` lines (M1+M2 self-contained
    /// trailer). The faithful dump↔load round-trip contract.
    #[test]
    fn parse_schema_dump_splits_body_and_trailer_entries() {
        let dumped = format!(
            "CREATE TABLE widgets (id int);\n\n{SCHEMA_TRAILER_MARKER}\n\
             --   20240101000001\tabc123\tcreate widgets\n\
             --   20240101000002\tdef456\tadd index\n"
        );
        let (body, entries) = parse_schema_dump(&dumped).expect("parse");
        assert!(body.contains("CREATE TABLE widgets"), "body is the DDL: {body}");
        assert!(
            !body.contains(SCHEMA_TRAILER_MARKER),
            "the trailer marker is NOT part of the body: {body}"
        );
        assert_eq!(
            entries,
            vec![
                TrailerEntry {
                    version: "20240101000001".to_string(),
                    checksum: "abc123".to_string(),
                    name: "create widgets".to_string(),
                },
                TrailerEntry {
                    version: "20240101000002".to_string(),
                    checksum: "def456".to_string(),
                    name: "add index".to_string(),
                },
            ]
        );
    }

    /// A dump with NO trailer (e.g. a hand-written schema) yields the whole content
    /// as the body and an empty entry list (load restores the DDL, journals nothing).
    #[test]
    fn parse_schema_dump_without_trailer_is_all_body() {
        let (body, entries) = parse_schema_dump("CREATE TABLE t (id int);\n").expect("parse");
        assert_eq!(body, "CREATE TABLE t (id int);\n");
        assert!(entries.is_empty());
    }

    /// M3 RED: a DDL body that legitimately CONTAINS the trailer-marker text in a
    /// comment must NOT be split there. The split is anchored to the marker's own
    /// line and takes the LAST occurrence (the real trailer, always at the end), so
    /// the in-body comment stays in the DDL and the trailer is parsed from the end.
    /// Pre-fix (`content.find` raw substring) this split at the FIRST hit, truncating
    /// the DDL and parsing the in-body comment as the trailer — RED.
    #[test]
    fn parse_schema_dump_marker_inside_ddl_body_is_not_a_split_point() {
        let dumped = format!(
            "CREATE TABLE notes (id int);\n\
             -- a comment mentioning {SCHEMA_TRAILER_MARKER} inline\n\
             CREATE TABLE more (id int);\n\n\
             {SCHEMA_TRAILER_MARKER}\n--   20240101000001\tabc\tcreate notes\n"
        );
        let (body, entries) = parse_schema_dump(&dumped).expect("parse");
        assert!(body.contains("CREATE TABLE notes"), "first table kept: {body}");
        assert!(body.contains("CREATE TABLE more"), "DDL after the in-body marker kept: {body}");
        assert!(
            body.contains("a comment mentioning"),
            "the in-body comment line survives: {body}"
        );
        assert_eq!(entries.len(), 1, "only the REAL trailer at the end is parsed");
        assert_eq!(entries[0].version, "20240101000001");
        assert_eq!(entries[0].checksum, "abc");
    }

    /// M2 RED: a malformed/truncated trailer line (not `--   <v>\t<c>\t<n>`) is a
    /// clean non-zero error, never a silently-dropped checksum/name. Pre-fix the
    /// version-only `--   <v>` parse silently accepted it (empty checksum/name) —
    /// the orphan-drift / lying-status class. RED.
    #[test]
    fn parse_schema_dump_malformed_trailer_line_is_an_error() {
        let dumped = format!(
            "CREATE TABLE t (id int);\n\n{SCHEMA_TRAILER_MARKER}\n--   20240101000001\n"
        );
        let err = parse_schema_dump(&dumped).unwrap_err();
        assert!(
            matches!(err, RunError::LoadCmd(_)),
            "a malformed trailer line is a clean LoadCmd error, got {err:?}"
        );
    }

    /// The restore sanitizer drops psql backslash META-COMMANDS (e.g. pg_dump ≥17's
    /// `\restrict`/`\unrestrict`) and the pg_dump session-`SET`/`set_config` preamble
    /// (which would abort the batch under pg_dump↔server version skew), while keeping
    /// the real schema DDL verbatim — including a multi-line CREATE statement.
    #[test]
    fn strip_psql_meta_commands_drops_backslash_and_session_settings() {
        let dump = "\\restrict ABCdef123\n\
                    SET statement_timeout = 0;\n\
                    SET transaction_timeout = 0;\n\
                    SELECT pg_catalog.set_config('search_path', '', false);\n\
                    SET row_security = off;\n\
                    CREATE TABLE public.widgets (\n    id integer NOT NULL,\n    name text\n);\n\
                    \\unrestrict ABCdef123\n";
        let out = strip_psql_meta_commands(dump);
        assert!(!out.contains("\\restrict"), "backslash directives stripped: {out}");
        assert!(!out.contains("\\unrestrict"), "backslash directives stripped: {out}");
        assert!(!out.contains("transaction_timeout"), "session SET stripped: {out}");
        assert!(!out.contains("statement_timeout"), "session SET stripped: {out}");
        assert!(!out.contains("set_config"), "set_config stripped: {out}");
        // The real DDL — including its inner lines — is preserved verbatim.
        assert!(out.contains("CREATE TABLE public.widgets ("), "DDL kept: {out}");
        assert!(out.contains("id integer NOT NULL"), "multi-line DDL body kept: {out}");
        assert!(out.contains("name text"), "multi-line DDL body kept: {out}");
    }

    /// `is_pg_dump_session_setting` recognises the session-preamble lines but never a
    /// real schema-`SET` (none exist in `--schema-only` output, but the predicate must
    /// not over-match a `SET`-prefixed identifier inside DDL).
    #[test]
    fn is_pg_dump_session_setting_matches_only_the_preamble() {
        assert!(is_pg_dump_session_setting("SET statement_timeout = 0;"));
        assert!(is_pg_dump_session_setting("  SET search_path = public;"));
        assert!(is_pg_dump_session_setting(
            "SELECT pg_catalog.set_config('search_path', '', false);"
        ));
        // Not a session setting: a CREATE statement that merely starts with "SE…".
        assert!(!is_pg_dump_session_setting("CREATE TABLE settings (id int);"));
        // An unknown GUC is NOT in the allowlist → not stripped (kept verbatim).
        assert!(!is_pg_dump_session_setting("SET my_custom_guc = 1;"));
    }

    /// L1 RED: the over-strip guard. A line that STARTS with an allowlisted-keyword
    /// `SET …` but does NOT end in `;` (a multi-line statement's first line) must NOT
    /// be recognised as a droppable single-statement preamble — dropping only its
    /// first line would corrupt the body. Pre-fix (no `;`-terminator requirement)
    /// this returned `true` for the un-terminated first line — RED.
    #[test]
    fn is_pg_dump_session_setting_requires_single_statement_semicolon() {
        // An allowlisted keyword but the statement continues onto the next line
        // (no trailing `;`) — must NOT be treated as a droppable preamble line.
        assert!(!is_pg_dump_session_setting("SET search_path ="));
        assert!(!is_pg_dump_session_setting("SET statement_timeout = 0"));
        // The real, self-terminated preamble line is still recognised.
        assert!(is_pg_dump_session_setting("SET search_path = public;"));
    }
}
