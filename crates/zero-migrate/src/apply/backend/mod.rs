//! The `MigrationBackend` dialect seam.
//!
//! The executor's apply/rollback **orchestration** — partition versioned vs
//! repeatable, the drift/tamper gate, squash/expand gates, `order_pending`, the
//! FIRST/SECOND pass, the repeatable phase, rollback selection + reverse-topo
//! ordering — is dialect-agnostic and stays single-sourced in
//! [`crate::apply::executor`]. Everything the orchestration touches that is
//! **dialect-coupled** lives behind this trait:
//!
//! - **connection / session I/O** — the project lock
//!   (`pg_advisory_lock(hashtext($1))`), the GUC snapshot/restore
//!   (`current_setting`/`set_config`), the unconditional `RESET ROLE`, and
//!   transaction begin/commit/rollback;
//! - **the per-migration confined apply** — the txn path (`BEGIN; SET LOCAL …;
//!   SET LOCAL ROLE migrator; <up>; RESET ROLE; INSERT journal; COMMIT`), the
//!   non-txn two-phase path, the non-txn crash recovery, and the rollback `down`;
//! - **journal row I/O** — the shared-sequence net-state reads
//!   (`applied`/`superseded_versions`/`latest_completed_checksums`), the
//!   immutability bootstrap, and the event inserts — exposed as **dialect-neutral
//!   owned row structs** (`AppliedEntry`, …), never a `compio_postgres::Row`;
//! - **parse-time non-txn idempotency validation** — PG calls `pg_query::parse`
//!   directly; a SQLite backend rejects `transaction:false` at the dialect
//!   boundary instead, so this MUST sit behind the trait (no raw `pg_query::parse`
//!   in the generic `apply_locked` body);
//! - **drift schema introspection** — `snapshot_schema` over
//!   `information_schema`/`pg_catalog` (PG) vs `sqlite_master` + PRAGMAs (SQLite);
//!   the checksum/tamper comparison itself is dialect-agnostic and stays generic
//!   ([`crate::apply::drift::check_checksum_drift`]).
//!
//! [`PostgresBackend`], [`sqlite::SqliteBackend`], and [`MysqlBackend`] are the live
//! implementations. Postgres remains the richest regression bar; SQLite and MySQL
//! provide dialect-specific session, journal, drift, and DML behavior behind the
//! same orchestration trait, without forking the generic executor.
//!
//! The trait is used through **static dispatch** (`<B: MigrationBackend>`), so
//! native `async fn` in trait (Rust ≥ 1.75) is used directly — no boxing, no
//! `dyn`, no `async-trait` allocation on the apply hot path.

pub mod capability;
// The PG backend module compiles on the PG seam cfg (`pg_seam`, emitted by build.rs
// from `host-pg`): its generic core (`PostgresBackend<D>` + the `SqlSession` trait +
// the neutral seam types) names no driver-concrete type — a host driver (the napi
// `pg` shell) supplies the `SqlSession` impl.
#[cfg(pg_seam)]
pub mod postgres;
// The MySQL backend rides the SAME `driver::SqlSession` seam as Postgres (only its
// dialect SQL differs), so it compiles on the seam cfg (`pg_seam`, emitted by
// build.rs from `host-pg`). SQLite, by contrast, is in-process (`rusqlite`) and is
// always present.
#[cfg(pg_seam)]
pub mod mysql;
pub mod sqlite;

pub use capability::{
    BackfillError, BackfillOutcome, BackfillSpec, DryRunError, DryRunReport, MigrationResult,
    OnlineSchemaChange, SeedError, ShadowConfig, ShadowDryRun,
};
#[cfg(pg_seam)]
pub use mysql::{
    MysqlBackend, MysqlInflightDdlMarker, MysqlInflightRecoveryError, MysqlInflightRecoveryOutcome,
    MysqlInflightResolution,
};
#[cfg(pg_seam)]
pub use postgres::PostgresBackend;

use std::future::Future;
use std::pin::Pin;

use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::apply::drift::DriftError;
use crate::apply::executor::{ApplyError, RollbackError};
use crate::apply::journal::{self, AppliedEntry, JournalError};
use crate::conn::ExecutorConfig;
use crate::model::migration::{Checksum, Migration, MigrationId};
use crate::model::snapshot::SchemaSnapshot;
use crate::render::plan::{DatabaseRequirements, TableRebuildSpec};
use crate::render::step::{
    AlterColumnTypeStep, AlterPrimaryKeyStep, BindValue, SynchronizeIdentityStep,
};
use crate::schema::query::SqlDialect;

/// The Postgres session GUCs the backend restores on exit so its per-apply
/// settings never leak onto the pooled/long-lived connection.
///
/// The generic executor sees this only as
/// [`MigrationBackend::SessionSnapshot`] and never inspects the fields.
#[derive(Debug, Clone, Default)]
pub struct PgSessionSnapshot {
    /// PG `statement_timeout` GUC text (e.g. `"60s"`). Empty for a backend that
    /// has no such setting.
    pub statement_timeout: String,
    /// PG `lock_timeout` GUC text.
    pub lock_timeout: String,
    /// PG `search_path` GUC text.
    pub search_path: String,
}

/// How a backend renders a positional bind placeholder in the SQL it issues.
///
/// The lock/journal/DML SQL a backend owns is Postgres-flavoured `$N` today
/// ([`Numbered`](PlaceholderStyle::Numbered)). A MySQL backend renders the
/// anonymous `?` ([`Question`](PlaceholderStyle::Question)) — the placeholder
/// style is a **backend concern**, consulted BEFORE any SQL crosses the
/// [`SqlSession`](crate::driver::SqlSession) seam, so the generic executor never
/// bakes a dialect's placeholder into shared SQL.
///
/// Exposed via [`MigrationBackend::placeholder_style`] +
/// [`MigrationBackend::placeholder`]; each backend's own leaves render their SQL
/// with their native style directly (PG session leaves write `$1` literally),
/// and a cross-dialect helper can render positionally through this hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderStyle {
    /// Postgres: the 1-based numbered form `$1`, `$2`, … (also SQLite's `?1`
    /// numbered form is rendered elsewhere via `render::dml`).
    Numbered,
    /// MySQL: the anonymous positional `?` (order-of-appearance binding).
    Question,
}

/// Read-only progress evidence for one resumable plan backfill.
///
/// `version` is the stable journal/progress identity of the lowered backfill
/// step. `checksum` is optional for compatibility with progress tables created
/// before the checksum column existed; a missing value is treated as drift by
/// plan-status reconciliation. `complete` does not by itself mean applied: the
/// ordinary `schema_migrations` completed event remains the source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillProgressEntry {
    /// Stable plan-step identity stored as `backfill_id`.
    pub version: String,
    /// Authoritative artifact checksum recorded when the backfill began.
    pub checksum: Option<String>,
    /// Whether the progress row reached its tail.
    pub complete: bool,
}

impl PlaceholderStyle {
    /// Render the `n`-th (1-based) positional bind placeholder in this style.
    /// `$n` for [`Numbered`](Self::Numbered); `?` for [`Question`](Self::Question)
    /// (MySQL binds positionally, so the index is implicit).
    #[must_use]
    pub fn render(self, n: usize) -> String {
        match self {
            Self::Numbered => format!("${n}"),
            Self::Question => "?".to_string(),
        }
    }
}

pub type JournalFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, JournalError>> + 'a>>;

/// Cross-deploy pending-contract obligations capability.
///
/// Backends return `Some(&dyn CrossDeployObligations)` only when they can open
/// and discharge cross-deploy obligations. If
/// [`MigrationBackend::pending_contracts`] returns `None`, this capability is
/// structurally absent: reads are empty and writes are no-ops/unreachable routing
/// for that backend.
pub trait CrossDeployObligations {
    /// Read the OUTSTANDING cross-deploy pending-contract obligations —
    /// the apply-time interlock read-back + the `status` orphan/blocked source.
    /// No-op iff [`MigrationBackend::pending_contracts`] is `None`.
    fn outstanding_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::PendingContract>>;

    /// Read terminal pending-contract tombstones for status and dependency
    /// enforcement.
    fn resolved_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::ResolvedPendingContract>>;

    /// Inspect the live table shape before destructive resolution SQL runs.
    fn pending_contract_shape<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        contract: &'a journal::PendingContract,
    ) -> JournalFuture<'a, journal::PendingContractShape>;

    /// Open a `pending` cross-deploy obligation AND, when a
    /// [`journal::DeployRecoveryScope`] is supplied, its `in_progress`
    /// deploy-scoped recovery marker — in ONE transaction. No-op iff
    /// [`MigrationBackend::pending_contracts`] is `None`.
    fn record_pending_contract_with_recovery<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        rec: journal::PendingContractRecord<'a>,
        scope: Option<journal::DeployRecoveryScope<'a>>,
    ) -> JournalFuture<'a, bool>;

    /// Discharge an obligation by APPENDING a `resolved` row (never a delete —
    /// history is append-only). No-op iff [`MigrationBackend::pending_contracts`]
    /// is `None`.
    fn resolve_pending_contract<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        pc: &'a journal::PendingContract,
        resolution: journal::Resolution,
        by: &'a str,
    ) -> JournalFuture<'a, ()>;

    /// Promote a WHOLE deploy's recovery markers to `committed` in ONE atomic
    /// transaction. No-op iff [`MigrationBackend::pending_contracts`] is
    /// `None`.
    fn mark_deploy_recovery_committed_batch<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        deploy_id: &'a str,
        pending_versions: &'a [String],
        by: &'a str,
    ) -> JournalFuture<'a, ()>;

    /// Mark a deploy-scoped recovery obligation `reconciled` (APPEND a
    /// `reconciled` row). No-op iff [`MigrationBackend::pending_contracts`] is
    /// `None`.
    fn mark_deploy_recovery_reconciled<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        deploy_id: &'a str,
        pending_version: &'a str,
        by: &'a str,
    ) -> JournalFuture<'a, ()>;

    /// Read the net-`in_progress` deploy-recovery markers whose obligation is
    /// still outstanding. Empty iff [`MigrationBackend::pending_contracts`] is
    /// `None`.
    fn outstanding_deploy_recoveries<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::DeployRecovery>>;
}

/// How many non-blocking project-lock attempts an acquisition makes before it
/// reports the lock busy.
///
/// FIXED and small on purpose: retrying until the lock is free is the unbounded
/// wait the non-blocking acquisition exists to avoid, only spelled with more round
/// trips.
#[cfg(pg_seam)]
pub(crate) const PROJECT_LOCK_TRY_ATTEMPTS: u32 = 3;

/// How long a non-blocking acquisition pauses between those attempts. Sized to
/// cover the gap between two of a deploy's statements, not the deploy.
#[cfg(pg_seam)]
pub(crate) const PROJECT_LOCK_TRY_BACKOFF: std::time::Duration =
    std::time::Duration::from_millis(200);

/// One session reported holding the project lock when an acquisition found it
/// taken.
///
/// Carries no duration on purpose. Neither `pg_locks` nor
/// `performance_schema.metadata_locks` records an acquisition timestamp, and every
/// timestamp the surrounding session views offer ages the holder's session or its
/// current statement, neither of which is the age of the lock -- reporting one as
/// "held for" would be a fabricated number an operator would act on.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct ProjectLockHolder {
    /// The holding session's id: a PostgreSQL backend pid, a MySQL connection id
    /// (what `KILL` takes). Either way, the handle an operator acts on.
    pub pid: i64,
    /// Who the holding session is. PostgreSQL reports its `application_name`, when
    /// it set one; MySQL has no such setting, so it reports the holder's account as
    /// `user@host`.
    pub application_name: Option<String>,
    /// The holder's activity state: PostgreSQL's `active` / `idle in transaction`,
    /// or MySQL's command and state (`Query: executing`, `Sleep`).
    pub state: Option<String>,
    /// The holder's current statement. Absent when it is running none, and on
    /// PostgreSQL also when the reading role may not see other sessions' statement
    /// text.
    pub query: Option<String>,
}

/// What a non-blocking project-lock acquisition found.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ProjectLockAcquisition {
    /// The lock is held by this session and must be released as usual.
    Acquired,
    /// A peer holds the lock. Nothing was locked, so there is nothing to release.
    Busy(Vec<ProjectLockHolder>),
}

/// One backend's answer about a single precondition asked of the PRE-PLAN
/// database, before any of the plan's steps has run.
///
/// [`Abstain`](Self::Abstain) is not "met". It says this backend has no
/// plan-level evaluator, so the plan-wide phase must say nothing at all and leave
/// the per-migration seam as the only judge - which is what keeps a backend
/// without a catalog to consult behaving exactly as it does today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanPreconditionVerdict {
    /// No plan-level evaluator on this backend. The plan-wide phase declines to
    /// form an opinion.
    Abstain,
    /// The assertion HOLDS against the pre-plan database.
    Met,
    /// The assertion is UNMET. `blockers` carries the object descriptions the two
    /// blocked-column assertions return, so the refusal can name what the
    /// database says is in the way; empty for every other variant.
    Unmet {
        /// What the catalog says blocks the operation, when the assertion can say.
        blockers: Vec<String>,
    },
}

/// The dialect seam over execution I/O, journal I/O, parse-time non-txn
/// validation, and drift introspection.
///
/// See the module docs for the full rationale. The methods are the EXACT pre-seam
/// Postgres code, moved verbatim, so the existing PG suite is the regression bar.
#[allow(async_fn_in_trait)]
pub trait MigrationBackend {
    /// Opaque backend-owned session state. The generic executor only
    /// round-trips this value from [`snapshot_session`](Self::snapshot_session)
    /// to [`restore_session`](Self::restore_session).
    type SessionSnapshot;

    /// The SQL dialect this backend targets. Drives the dialect-boundary rejects
    /// in the generic body (e.g. `transaction:false` on SQLite).
    fn dialect(&self) -> SqlDialect;

    /// The positional bind placeholder style this backend's SQL uses (`$N` on
    /// Postgres, `?` on MySQL). Placeholder style is a **backend concern** so no
    /// dialect placeholder is ever baked into the shared executor's SQL — a
    /// backend renders its lock/journal/DML binds in its own style before the SQL
    /// crosses the [`SqlSession`](crate::driver::SqlSession) seam. Postgres backends
    /// keep the numbered `$N` form; a MySQL backend overrides to
    /// [`PlaceholderStyle::Question`].
    fn placeholder_style(&self) -> PlaceholderStyle {
        PlaceholderStyle::Numbered
    }

    /// Render the `n`-th (1-based) positional placeholder in this backend's
    /// [`placeholder_style`](Self::placeholder_style). Convenience over
    /// [`PlaceholderStyle::render`] so a cross-dialect SQL builder can ask the
    /// backend directly.
    fn placeholder(&self, n: usize) -> String {
        self.placeholder_style().render(n)
    }

    /// Verify the live database features required by a lowered IR plan before
    /// any authored plan step executes. Backends opt into concrete feature
    /// probes; an empty requirement set is universally valid, while an unknown
    /// non-empty set fails closed by default.
    async fn verify_database_requirements(
        &self,
        requirements: &DatabaseRequirements,
    ) -> Result<(), ApplyError> {
        if requirements.is_empty() {
            return Ok(());
        }
        let features = requirements
            .iter()
            .map(|feature| feature.description())
            .collect::<Vec<_>>()
            .join(", ");
        Err(ApplyError::Backend(format!(
            "{:?} backend cannot verify required database feature(s): {features}",
            self.dialect()
        )))
    }

    /// Whether this backend can commit DDL atomically with its journal row inside
    /// one transaction. Postgres/SQLite return true; auto-commit DDL dialects
    /// return false and force the two-phase started/completed path for every
    /// migration.
    fn ddl_is_transactional(&self) -> bool;

    /// Whether a migration must take the two-phase non-transactional apply path.
    ///
    /// Existing Postgres/SQLite behavior reduces to `!m.flags.transactional`
    /// for versioned migrations because both backends report transactional DDL.
    /// Repeatables are always forced through the transactional apply path,
    /// matching the pre-P2a invariant.
    fn uses_two_phase_path(&self, m: &Migration) -> bool {
        if m.flags.repeatable {
            return false;
        }
        !self.ddl_is_transactional() || !m.flags.transactional
    }

    // -- connection / session I/O -------------------------------------------

    /// Acquire the project apply-serialization lock (PG `pg_advisory_lock`;
    /// MySQL `GET_LOCK`).
    async fn acquire_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError>;

    /// Release the project apply-serialization lock (PG `pg_advisory_unlock`;
    /// MySQL `RELEASE_LOCK`).
    async fn release_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError>;

    /// Take the project lock without waiting out a peer's deploy, reporting who
    /// holds it when it cannot be taken.
    ///
    /// Read-only callers use this instead of
    /// [`acquire_project_lock`](Self::acquire_project_lock): a deploy holds the
    /// lock for its whole run, so a reader that waits inherits that wall clock with
    /// no timeout to end it. Contention is an OUTCOME here, not an error, because
    /// the callers that need it decide exit codes and must not read "a peer is
    /// deploying" as "this migration set is dirty".
    ///
    /// PostgreSQL and MySQL both override it: `pg_advisory_lock` waits forever, and
    /// `GET_LOCK` waits out a ten second budget and then reports the contention as
    /// an error, which every caller turns into a failing exit code. The default
    /// delegates to the blocking acquisition, which is what SQLite wants -- its
    /// try-lock spin is already bounded by the configured lock timeout, and a
    /// single-file database has no peer session to name.
    ///
    /// # Errors
    /// Whatever the backend's acquisition reports; contention itself is not one.
    async fn try_acquire_project_lock(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<ProjectLockAcquisition, ApplyError> {
        self.acquire_project_lock(cfg)
            .await
            .map(|()| ProjectLockAcquisition::Acquired)
    }

    /// Snapshot the session settings the apply will override, for restore on exit.
    ///
    /// The supplied session must be dedicated to zero-migrate for the duration of
    /// the call and must not carry caller-owned transactional work. A backend that
    /// can detect an active transaction must reject it here, before any session
    /// setting or author SQL is executed. If this method fails, the generic
    /// executor releases any lock it acquired and aborts the apply.
    async fn snapshot_session(&self) -> Result<Self::SessionSnapshot, ApplyError>;

    /// Restore the settings captured by [`snapshot_session`](Self::snapshot_session).
    /// Includes the leading unconditional role reset where the backend has roles.
    /// A restoration error is surfaced when the apply itself otherwise succeeded,
    /// so a connection with leaked settings is never reported as a clean success.
    async fn restore_session(&self, snap: &Self::SessionSnapshot) -> Result<(), ApplyError>;

    /// Drop any per-apply privilege confinement back to the connecting role,
    /// unconditionally. Postgres runs `RESET ROLE`; SQLite no-ops; a session-role
    /// backend supplies its own reset verb. Best-effort: logs on failure, never
    /// fails the apply.
    async fn reset_role_best_effort(&self);

    // -- per-migration confined apply ---------------------------------------

    /// Apply ONE migration. The backend owns the atomicity decision:
    ///
    /// - transactional-DDL + `transactional:true`: `BEGIN; SET LOCAL …;
    ///   SET LOCAL ROLE migrator; <up>; RESET ROLE; INSERT journal; (edges);
    ///   COMMIT`;
    /// - non-transactional-DDL OR `transactional:false`: two-phase `started`
    ///   marker → run the confined `<up>` → immutable `completed` row + clear
    ///   marker. When `had_inflight`, the backend must either prove replay safe
    ///   and recover, or preserve the marker and fail closed for audited repair.
    ///
    /// Returns `true` iff a two-phase apply recovered a prior inflight marker.
    async fn apply_one(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
        had_inflight: bool,
        supersedes: &[&str],
        kind: &str,
    ) -> Result<bool, ApplyError>;

    /// Roll back ONE migration transactionally: `BEGIN; SET LOCAL …;
    /// SET LOCAL ROLE migrator; <down>; RESET ROLE; INSERT rolled_back; COMMIT`.
    /// `down` + the `rolled_back` append commit atomically.
    async fn rollback_one_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError>;

    /// Execute an author's lowered inverse plan as one rollback unit, then append
    /// exactly one `rolled_back` event using the FORWARD migration identity.
    ///
    /// The generic rollback planner admits only transactional parameterized DML
    /// steps here. Backends must bind their values through the same native-bind
    /// seam used by [`Self::run_dml_step`]; the inverse is never rendered or
    /// interpolated into SQL text. The mutation(s) and the forward identity's
    /// journal event must commit atomically.
    async fn rollback_plan_transactional(
        &self,
        cfg: &ExecutorConfig,
        forward: &Migration,
        inverse_steps: &[crate::render::step::PlanStep],
        applied_by: &str,
    ) -> Result<(), RollbackError>;

    // -- parse-time validation ----------------------------------------------

    /// Validate the recovery contract for a **non-transactional** migration's
    /// `up`. PG parses with `pg_query` and enforces
    /// `IF NOT EXISTS` on `CREATE INDEX CONCURRENTLY` / `ALTER TYPE … ADD VALUE`
    /// and forbids bare DML; MySQL preserves ambiguous inflight state and fails
    /// closed instead of replaying generated DDL; a SQLite backend rejects
    /// `transaction:false` outright (no non-txn DDL exists on SQLite).
    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError>;

    /// Why this migration's `down` cannot run inside the transaction the rollback
    /// leaf opens, or `None` when nothing in it objects.
    ///
    /// Gate (5b) of `plan_rollback` consults the `transaction` flag, which is the
    /// author's DECLARATION. This is what the dialect says after reading the reverse
    /// SQL itself, so a migration declaring `transaction: true` and then reversing
    /// itself with a statement the server refuses inside a transaction block is caught
    /// during planning instead of mid-batch.
    ///
    /// Required rather than defaulted, like `validate_non_txn` above: a new backend
    /// must answer this deliberately. Inheriting `None` would be a silent fail-open
    /// for a dialect nobody has thought about yet.
    fn non_transactional_down_reason(&self, m: &Migration) -> Option<String>;

    // -- journal row I/O (dialect-neutral owned rows) -----------------------

    /// True iff the journal meta objects already exist. MUST NOT create them, and
    /// (SQLite) MUST NOT create/attach the journal file as a side effect.
    async fn journal_exists(&self, cfg: &ExecutorConfig) -> Result<bool, JournalError>;

    /// Bootstrap the journal + immutability constructs (idempotent).
    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError>;

    /// Versions with an unresolved backend-specific rollback marker.
    ///
    /// MySQL uses a durable side-table because its `down` DDL auto-commits and an
    /// interrupted unwind can leave an unverified partially-reverted shape.
    /// Transactional rollback backends have no such artifact and inherit the
    /// empty result.
    async fn unresolved_rollback_markers(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        Ok(Vec::new())
    }

    /// The net-applied + lone-`started` journal entries (the drift/pending input).
    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError>;

    /// Versions whose latest journal event is a rollback, ordered by version.
    async fn net_rolled_back_versions(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError>;

    /// Read resumable backfill progress rows, if the backend has created its
    /// progress table. An absent table returns an empty list. The reader must not
    /// bootstrap or otherwise mutate progress state because status is read-only.
    async fn backfill_progress(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<BackfillProgressEntry>, JournalError>;

    /// The versions covered by a net-applied squash (the supersession net-state).
    async fn superseded_versions(&self, cfg: &ExecutorConfig) -> Result<Vec<String>, JournalError>;

    /// The latest journaled `completed` checksum per identity — the repeatable
    /// re-run oracle.
    async fn latest_completed_checksums(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<std::collections::HashMap<String, String>, JournalError>;

    /// The cross-deploy pending-contract capability.
    ///
    /// `Some(&dyn CrossDeployObligations)` for a backend that can open and
    /// discharge pending-contract rows and deploy-recovery markers. `None` for a
    /// backend that structurally has no cross-deploy obligation partition; for
    /// such a backend pending-contract reads are empty and writes are no-ops by
    /// construction.
    fn pending_contracts(&self) -> Option<&dyn CrossDeployObligations>;

    // -- DB-coupled validation / introspection ------------------------------

    /// The checksum/tamper drift report over the journal (dialect-agnostic
    /// comparison; the journal read underneath is dialect-coupled, hence here).
    async fn check_checksum_drift(
        &self,
        cfg: &ExecutorConfig,
        migrations: &[Migration],
    ) -> Result<crate::apply::drift::ChecksumDriftReport, DriftError>;

    /// Introspect the live schema for the structural drift surface (PG
    /// `information_schema`/`pg_catalog`; SQLite `sqlite_master` + PRAGMAs).
    async fn snapshot_schema(&self, cfg: &ExecutorConfig) -> Result<SchemaSnapshot, DriftError>;

    // -- preconditions (DB-coupled) -----------------------------------------

    /// Evaluate a migration's preconditions read-only under the apply lock.
    /// Behind the trait so the generic body never holds a concrete connection;
    /// the PG impl delegates to [`crate::apply::precondition::evaluate`].
    async fn evaluate_preconditions(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<crate::apply::executor::PreconditionVerdict, ApplyError>;

    /// Evaluate ONE precondition against the PRE-PLAN database, before any of the
    /// plan's steps has run - the seam behind
    /// [`crate::apply::plan_precondition`]'s plan-wide phase.
    ///
    /// Read-only, and called under the project advisory lock, so the state it
    /// reads is the state the first step will meet.
    ///
    /// The default ABSTAINS, and that is the honest answer rather than a stub. A
    /// backend with no precondition evaluator has no plan-level opinion to offer,
    /// and its per-migration seam already fails closed on a declared precondition
    /// ([`Self::evaluate_preconditions`]). Abstaining leaves SQLite and MySQL
    /// byte-identical: the same refusal, at the same seam, with the same wording.
    /// Only the PostgreSQL impl answers, routing to the SAME
    /// `apply::precondition` body the per-migration seam uses, so the two cannot
    /// disagree about one assertion.
    ///
    /// # Errors
    /// [`ApplyError::PreconditionFailed`] when the check cannot be EVALUATED
    /// (guard denial, invalid identifier, DB error) - fail-closed, exactly as the
    /// per-migration seam is.
    async fn evaluate_plan_precondition(
        &self,
        _cfg: &ExecutorConfig,
        _version: &str,
        _check: &crate::model::precondition::Precondition,
    ) -> Result<PlanPreconditionVerdict, ApplyError> {
        Ok(PlanPreconditionVerdict::Abstain)
    }

    /// Describe the objects that would make the database REFUSE a bare
    /// `DROP COLUMN` of `column` on `table`, or an empty list when the drop would
    /// be accepted.
    ///
    /// Read-only, and called under the apply lock before a plan runs so a rename
    /// that cannot finish is refused before it opens a contract obligation nothing
    /// can discharge.
    ///
    /// The default answers "nothing blocks it". That is the honest answer for a
    /// backend with no such dependency concept rather than a stub: SQLite rewrites
    /// dependent expressions itself during its table rebuild, and the MySQL leg
    /// never reaches an expand-contract rename, because the declarative differ
    /// refuses one at plan time
    /// (`DeclarativeError::MysqlRenameColumnUnsupported`) and the IR lane answers
    /// `UnsupportedInV1`. That refusal is what makes the default safe here; before
    /// it existed, a MySQL rename DID reach this default and took the "nothing
    /// blocks it" answer on a question nobody had asked the server. Only the
    /// PostgreSQL impl consults a catalog, using the predicate
    /// `tests/pg_column_drop_dependency_oracle.rs` measures against a live server.
    async fn blocking_column_dependents(
        &self,
        _cfg: &ExecutorConfig,
        _table: &str,
        _column: &str,
    ) -> Result<Vec<String>, ApplyError> {
        Ok(Vec::new())
    }

    /// Describe what would make the database REFUSE an
    /// `ALTER TABLE … ALTER COLUMN … TYPE` of `column` on `table`, or an empty list
    /// when the retype would be accepted.
    ///
    /// A SEPARATE question from [`Self::blocking_column_dependents`], and the two
    /// answers really do differ. Measured against a live PostgreSQL beside the
    /// server's own verdict, one table per companion object: a column another
    /// table's FOREIGN KEY points at BLOCKS a drop and is ACCEPTED for a retype, so
    /// reusing the drop predicate here would refuse a migration the server honours.
    /// In the other direction a retype is blocked by partition-key membership and
    /// by column inheritance, neither of which is a dependency and neither of which
    /// a drop predicate would ever look at.
    ///
    /// Read-only, and called under the apply lock immediately before the retype, so
    /// a statement the server will refuse never runs beside ops that would commit.
    ///
    /// The default answers "nothing blocks it", for the same reason the drop
    /// predicate's default does: SQLite reconciles a type change by rebuilding the
    /// table rather than by altering a column, and the MySQL leg refuses
    /// `setColumnType` at the lower. Only the PostgreSQL impl consults a catalog,
    /// using the predicate `tests/pg_column_retype_dependency_oracle.rs` measures
    /// against a live server.
    async fn column_type_change_blockers(
        &self,
        _cfg: &ExecutorConfig,
        _table: &str,
        _column: &str,
    ) -> Result<Vec<String>, ApplyError> {
        Ok(Vec::new())
    }

    // -- squash (DB-coupled supersession journal write) ---------------------

    /// Journal a **squash** as a `completed` `kind='squash'` event WITHOUT running
    /// its `up`, plus the `S → v_i` supersession edges — the dialect-coupled write
    /// behind the generic [`crate::ops::squash::squash`] (multi-engine abstraction).
    ///
    /// Called only after the generic body has verified, under the project lock,
    /// that ALL of `supersedes` are net-applied (the existing-DB squash path). The
    /// PG impl delegates to [`crate::apply::journal::record_baseline`] with
    /// `kind='squash'`; a SQLite impl writes the same row+edges atomically through
    /// its actor. The connection / `pg_advisory_lock` never cross this surface.
    ///
    /// **MySQL implements this method as a refusal**, and that is narrower than it
    /// sounds: it is the records-not-run primitive that is unwired there (it shares
    /// the baseline machinery, which MySQL also refuses). The FRESH-path squash —
    /// where the squash's own `up` DOES run — is handled inline by the MySQL
    /// two-phase apply for a non-empty `supersedes`, edges included, so squashing a
    /// history forward works on all three targets and only adopting an
    /// already-applied history without running it does not.
    ///
    /// # Errors
    /// [`ApplyError::Db`] on a structured driver/write failure, or
    /// [`ApplyError::Backend`] for an intentionally textual dialect-level
    /// failure; the dialect-neutral [`crate::apply::executor::ApplyError`] is
    /// what crosses the trait.
    async fn record_squash(
        &self,
        cfg: &ExecutorConfig,
        squash_migration: &Migration,
        applied_by: &str,
        supersedes: &[&str],
    ) -> Result<(), ApplyError>;

    // -- declarative-only structured ops ------------------------------------

    /// Apply ONE structured SQLite 12-step table REBUILD atomically with
    /// confinement + journal it, the dialect-coupled drive behind the
    /// generic declarative apply path. A rebuild is NOT a plain `up` statement — it
    /// is an engine-mode structured operation (drop-stale-temp / CREATE new / copy /
    /// drop old / rename / replay captured dependents) with `foreign_keys` toggles
    /// straddling the transaction — so it cannot flow through
    /// [`apply_one`](Self::apply_one); the engine drives it
    /// here, after the destructive/approval gate it already runs for the rebuild's
    /// `destructive`-flagged journal migration.
    ///
    /// Rebuilds exist ONLY on the SQLite dialect: the SQLite differ emits them for
    /// the existing-table changes SQLite has no native `ALTER` for, and `plan.renames`
    /// is empty on SQLite so this is reached only with a SQLite-produced spec. The
    /// Postgres impl rejects it ([`ApplyError::Backend`]): PG never produces a
    /// `TableRebuild` (its differ uses native `ALTER` / expand-contract), so a rebuild
    /// reaching the PG backend is a routing bug, surfaced as a clear error rather than
    /// a silent pass.
    ///
    /// **Per-version approval scope (executor-layer defense in depth).** A
    /// rebuild on a populated table is destructive (drop + recreate + copy), so when
    /// `m.flags.destructive` (always true for a `TableRebuild` by construction,
    /// [`crate::render::declarative`]) the `scope` must admit `m.version` — mirroring
    /// [`PlanStep::approval_scope_version`](crate::render::step::PlanStep::approval_scope_version)'s
    /// rule. The engine's `apply_plan` gate runs first; this is the independent
    /// executor-layer check so a direct seam caller (driving `rebuild_one` without the
    /// engine) cannot bypass the per-version scope. Refused with
    /// [`ApplyError::ApprovalNotScoped`] BEFORE the rebuild touches the table.
    ///
    /// # Errors
    /// [`ApplyError::ApprovalNotScoped`] for a destructive rebuild whose version the
    /// `scope` does not admit; the dialect-neutral [`ApplyError::Backend`] on a rebuild
    /// failure (FK-check abort, confinement denial, DDL failure, or a poisoned
    /// connection — the SQLite transaction is rolled back, leaving the original table
    /// intact), or — on the PG backend — the unreachable-routing reject.
    async fn rebuild_one(
        &self,
        spec: &TableRebuildSpec,
        m: &Migration,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<(), ApplyError>;

    /// Apply one explicit primary-key add/replace/drop operation after checking
    /// its exact live-catalog preconditions under the project lock.
    ///
    /// The structured step is intentionally not rendered into ordinary DDL at
    /// lower time: each target must atomically combine the declared generation
    /// transition with its constraint change and must refuse unsafe inbound
    /// foreign-key dependencies.
    async fn alter_primary_key(
        &self,
        _cfg: &ExecutorConfig,
        _step: &AlterPrimaryKeyStep,
        _approval: crate::approval::Approval,
        _scope: &crate::approval::ApprovalScope,
        _applied_by: &str,
    ) -> Result<bool, ApplyError> {
        Err(ApplyError::Backend(
            "this backend does not implement explicit primary-key lifecycle operations".to_string(),
        ))
    }

    /// Retype one column on a dialect that spells the change by RESTATING the
    /// whole column definition, reading that definition from the live server
    /// under the project lock.
    ///
    /// Only MySQL needs this seam. PostgreSQL has `ALTER COLUMN … TYPE`, which
    /// carries one facet and leaves the rest alone, so its retype is ordinary
    /// rendered DDL; SQLite has no `ALTER COLUMN` at all and reconciles a retype
    /// through the differ's table rebuild. Both therefore keep the default
    /// refusal below rather than implementing this.
    async fn alter_column_type(
        &self,
        _cfg: &ExecutorConfig,
        _step: &AlterColumnTypeStep,
        _approval: crate::approval::Approval,
        _scope: &crate::approval::ApprovalScope,
        _applied_by: &str,
    ) -> Result<bool, ApplyError> {
        Err(ApplyError::Backend(
            "this backend does not restate column definitions to retype a column".to_string(),
        ))
    }

    /// Reconcile one imported integer identity column's live generator while
    /// the project migration lock is held. Implementations validate the exact
    /// target-specific generator association and journal the monotonic action.
    async fn synchronize_identity(
        &self,
        _cfg: &ExecutorConfig,
        _step: &SynchronizeIdentityStep,
        _applied_by: &str,
    ) -> Result<bool, ApplyError> {
        Err(ApplyError::Backend(
            "this backend does not implement identity synchronization".to_string(),
        ))
    }

    /// Drive a single **batched data backfill** step (`op.*` DSL,
    /// [`PlanStep::Backfill`](crate::render::step::PlanStep::Backfill)) through the
    /// dialect seam, journaling the spec's marker version. On Postgres this
    /// delegates to the backfill runner (writable-CTE windowed `UPDATE`). On SQLite
    /// the batched-backfill executor fails closed with a clear
    /// [`ApplyError::Backend`] rather than silently
    /// skipping the data transform (a SQLite-targeted batched backfill is a hard
    /// error, never a silent mis-apply).
    ///
    /// `lock_mode` mirrors the per-`Migration` apply: under
    /// [`LockMode::AlreadyHeld`](crate::apply::executor::LockMode) the caller already
    /// holds the project lock for the whole deploy (the PG per-batch
    /// `pg_advisory_xact_lock` inside the backfill is re-entrant under it).
    ///
    /// # Errors
    /// [`ApplyError::Backend`] on a backfill failure or the SQLite-unsupported
    /// arm; the failure is resumable from the last committed cursor on
    /// PG.
    ///
    /// **Per-version approval scope (executor-layer defense in depth).** A
    /// standalone batched backfill rewrites existing data, so it requires explicit
    /// approval and `scope` must admit its stable `version`. The data-mutating
    /// EXPAND backfill rides
    /// [`OnlineSchemaChange::run_online`],
    /// NOT this method.
    async fn run_backfill_step(
        &self,
        cfg: &ExecutorConfig,
        version: &MigrationId,
        checksum: &Checksum,
        spec: &BackfillSpec,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        lock_mode: crate::apply::executor::LockMode,
    ) -> Result<crate::apply::executor::ApplyOutcome, ApplyError>;

    /// Drive a single **parameterized DML** step (`op.*` DSL,
    /// [`PlanStep::Dml`](crate::render::step::PlanStep::Dml)) through the dialect seam.
    /// The `template` is executed with `binds` bound NATIVELY (`$n` on PG, `?n`
    /// on SQLite) — never string-interpolated, so a bind value can never alter
    /// statement structure. The step is journaled under `version` (its
    /// sub-version), so a re-run is a net-applied-skip (idempotent).
    ///
    /// `destructive` carries the step's data-loss flag (a `delete`). The
    /// implementation re-runs the approval gate as **defense in depth**: a
    /// destructive DML under any approval other than [`crate::approval::Approval::Approved`] is
    /// refused with [`ApplyError::ApprovalRequired`] BEFORE the template executes,
    /// mirroring the per-`Migration` gate in
    /// `apply_with_lock_backend`. The
    /// engine's [`apply_plan`](crate::engine::MigrationEngine::apply_plan) gate runs
    /// first; this is the independent executor-layer check so a direct seam caller
    /// cannot bypass it.
    ///
    /// **Per-version approval scope (executor-layer defense in depth).** On top
    /// of the coarse approval gate, a destructive DML runs ONLY if `scope` admits its
    /// `version` — mirroring
    /// [`PlanStep::approval_scope_version`](crate::render::step::PlanStep::approval_scope_version)'s
    /// rule for `Dml`. So a direct seam caller driving `run_dml_step` with blanket
    /// [`crate::approval::Approval::Approved`] but an [`crate::approval::ApprovalScope::Versions`] set that omits this
    /// `version` is refused with [`ApplyError::ApprovalNotScoped`] BEFORE the template
    /// executes — the executor-layer mirror of the engine's per-version scope gate.
    ///
    /// `checksum` is the authoritative checksum of the complete IR artifact. It
    /// includes typed bind values, so a same-version edit is refused as drift.
    ///
    /// Returns `true` if the step was applied this run, `false` if it was already
    /// net-applied (skipped).
    ///
    /// # Errors
    /// [`ApplyError::ApprovalRequired`] for a destructive DML without approval;
    /// [`ApplyError::ApprovalNotScoped`] for a destructive DML whose `version` the
    /// `scope` does not admit; [`ApplyError`] on a DML/journal failure or the
    /// SQLite-unsupported arm.
    #[allow(clippy::too_many_arguments)]
    async fn run_dml_step(
        &self,
        cfg: &ExecutorConfig,
        version: &MigrationId,
        checksum: &Checksum,
        name: &str,
        template: &str,
        binds: &[BindValue],
        target_schema: &str,
        target_table: &str,
        conflict_target: Option<&[String]>,
        mutates_data: bool,
        destructive: bool,
        owner_app: &str,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        lock_mode: crate::apply::executor::LockMode,
    ) -> Result<bool, ApplyError>;

    /// The **online schema-change capability** (multi-engine abstraction).
    ///
    /// `Some(&dyn OnlineSchemaChange)` for an engine that drives zero-downtime
    /// online operations (Postgres — expand-contract rename via dual-write trigger
    /// plus paged backfill; a `PgOnline` impl would own its connection
    /// **internally**, so the connection NEVER appears on this
    /// neutral trait surface). `None` for an engine with no online path (SQLite,
    /// where every existing-table change, rename included, is routed to a
    /// [`rebuild_one`](Self::rebuild_one), so its declarative `renames` set is
    /// structurally EMPTY).
    ///
    /// This REPLACES the old `expand_conn()` connection escape hatch:
    /// the generic declarative apply path branches on
    /// `online().is_some()`, never holding a concrete connection, and a backend
    /// with no online capability MUST receive an empty `renames` (asserted at the
    /// call site — a non-empty rename set with `online() == None` is a routing bug).
    fn online(&self) -> Option<&dyn crate::apply::backend::capability::OnlineSchemaChange>;

    /// The **shadow dry-run capability** (multi-engine abstraction).
    ///
    /// `Some(&dyn ShadowDryRun)` for an engine that can preview a migration batch
    /// against a throwaway shadow clone before the real apply (Postgres — a
    /// `PgShadow` impl would own its admin connection
    /// **internally**, so the connection NEVER appears on this neutral trait
    /// surface). `None` for an engine with no shadow path.
    ///
    /// This REPLACES the old `MigrationEngine::dry_run(admin_conn: &Client, …)` /
    /// `PostgresBackend::conn() -> &Client` escape hatch: the engine's
    /// [`dry_run`](crate::engine::MigrationEngine::dry_run) /
    /// [`dry_run_declarative`](crate::engine::MigrationEngine::dry_run_declarative)
    /// branch on `shadow().is_some()`, never holding a concrete connection, and a
    /// backend with no shadow capability surfaces a clear
    /// [`DryRunError::ShadowUnsupported`]
    /// rather than a false-success report.
    ///
    /// # Why SQLite is `None` (a deliberate capability gap, not a silent hole)
    ///
    /// The SQLite dev path applies only **TRUSTED descriptor-generated DDL** —
    /// there is no untrusted/raw SQLite author whose DDL would need previewing
    /// against a throwaway clone — and dev is **recoverable** (a local file the
    /// developer can re-create), so a pre-apply shadow dry-run adds little. The
    /// shadow exists to safely preview *untrusted/AI-authored* DDL before it
    /// touches a *durable* schema; neither condition holds on the SQLite dev leg.
    /// A future untrusted/prod non-PG engine WOULD provide a
    /// `ShadowDryRun`. So `None` here is honest: a caller that asks for a dry-run
    /// on SQLite gets the explicit `ShadowUnsupported` outcome, never a fake
    /// "dry-run passed".
    fn shadow(&self) -> Option<&dyn crate::apply::backend::capability::ShadowDryRun>;

    // -- baseline / adoption ------------------------------------------------

    /// Adopt the LIVE schema as the project's **baseline**
    /// (multi-engine abstraction) — a `kind='baseline'`, `completed` journal
    /// event recorded WITHOUT running `m`'s `up`. The single neutral baseline entry
    /// point that folds the two former dialect-specific baseline functions
    /// (`baseline(&Client, …)` and `SqliteBackend::baseline_sqlite`) behind ONE
    /// trait method, so no PG-`&Client`-typed baseline remains on the abstraction
    /// surface.
    ///
    /// First-entry-only: idempotent for the SAME version
    /// ([`BaselineOutcome::already_present`]); refuses if the journal already
    /// records a DIFFERENT net-applied migration (the engine already manages this
    /// DB) — fail-closed, nothing journaled. The Postgres impl additionally runs the
    /// baseline `up` through the guard (defense in depth) and serializes under the
    /// project advisory lock; the SQLite impl serializes structurally on its single
    /// migration actor.
    ///
    /// # Errors
    /// - [`BaselineError::Guard`] — the baseline SQL was denied (PG; held to the same
    ///   deny-list as any `up`).
    /// - [`BaselineError::AlreadyManaged`] / [`BaselineError::ConflictingBaseline`] —
    ///   not a first-entry DB.
    /// - [`BaselineError::Db`] / [`BaselineError::Journal`] — PG infrastructure
    ///   failures.
    /// - [`BaselineError::Backend`] — a non-PG (e.g. SQLite) backend's internal
    ///   failure, mapped onto the dialect-neutral arm. **MySQL also returns this
    ///   arm to say the operation is UNSUPPORTED**, not that it failed:
    ///   `"mysql backend: schema baseline/adoption is not supported on MySQL"`.
    ///
    /// SUPPORTED ON PostgreSQL AND SQLite ONLY. Every backend implements this
    /// method because the trait requires it; MySQL's implementation is a refusal
    /// stub, since adoption relies on the guard, advisory-lock and record-not-run
    /// machinery that is PG-specific and has no MySQL equivalent. Reading the impl
    /// list is therefore misleading — a trait impl existing is not the feature
    /// working, and this one is three lines that always return `Err`.
    async fn baseline_one(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<BaselineOutcome, BaselineError>;
}
