//! The `MigrationBackend` dialect seam (SQLite-parity design §2.0/§2.1, M3).
//!
//! The executor's apply/rollback **orchestration** — partition versioned vs
//! repeatable, the drift/tamper gate, squash/expand gates, `order_pending`, the
//! FIRST/SECOND pass, the repeatable phase, rollback selection + reverse-topo
//! ordering — is dialect-agnostic and stays single-sourced in
//! [`crate::executor`]. Everything the orchestration touches that is
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
//!   ([`crate::drift::check_checksum_drift`]).
//!
//! **P1 (this phase): Postgres is the FIRST AND ONLY impl.** [`PostgresBackend`]
//! is the regression bar — every method below moves the EXISTING executor /
//! journal / drift code behind the trait, behavior-identical. No SQLite code
//! exists yet; a `SqliteBackend` is added later (design §2.1.1) WITHOUT forking
//! the executor, because the orchestration is already generic over this trait.
//!
//! The trait is used through **static dispatch** (`<B: MigrationBackend>`), so
//! native `async fn` in trait (Rust ≥ 1.75) is used directly — no boxing, no
//! `dyn`, no `async-trait` allocation on the apply hot path.

pub mod postgres;
pub mod sqlite;

pub use postgres::PostgresBackend;

use std::future::Future;
use std::pin::Pin;

use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::conn::ExecutorConfig;
use crate::apply::drift::DriftError;
use crate::apply::executor::{ApplyError, RollbackError};
use crate::apply::journal::{self, AppliedEntry, JournalError};
use crate::model::migration::Migration;
use crate::model::snapshot::SchemaSnapshot;
use crate::render::plan::{BackfillSpec, SqliteRebuildSpec};
use crate::render::step::BindValue;
use zeroship_schema::query::SqlDialect;

/// The Postgres session GUCs the backend restores on exit so its per-apply
/// settings never leak onto the pooled/long-lived connection (H2).
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

pub type JournalFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, JournalError>> + 'a>>;

/// Cross-deploy pending-contract obligations capability.
///
/// Backends return `Some(&dyn CrossDeployObligations)` only when they can open
/// and discharge cross-deploy obligations. If
/// [`MigrationBackend::pending_contracts`] returns `None`, this capability is
/// structurally absent: reads are empty and writes are no-ops/unreachable routing
/// for that backend.
pub trait CrossDeployObligations {
    /// Read the OUTSTANDING cross-deploy pending-contract obligations (§2.0.3) —
    /// the apply-time interlock read-back + the `status` orphan/blocked source.
    /// No-op iff [`MigrationBackend::pending_contracts`] is `None`.
    fn outstanding_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::PendingContract>>;

    /// Open a `pending` cross-deploy obligation AND, when a
    /// [`journal::DeployRecoveryScope`] is supplied, its `in_progress`
    /// deploy-scoped recovery marker — in ONE transaction (PR9e). No-op iff
    /// [`MigrationBackend::pending_contracts`] is `None`.
    fn record_pending_contract_with_recovery<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        rec: journal::PendingContractRecord<'a>,
        scope: Option<journal::DeployRecoveryScope<'a>>,
    ) -> JournalFuture<'a, ()>;

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
    /// transaction (PR9e). No-op iff [`MigrationBackend::pending_contracts`] is
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

/// The dialect seam over execution I/O, journal I/O, parse-time non-txn
/// validation, and drift introspection (design §2.0/§2.1, M3).
///
/// See the module docs for the full rationale. P1 has exactly one impl,
/// [`PostgresBackend`]; the methods are the EXACT pre-seam Postgres code, moved
/// verbatim, so the existing PG suite is the regression bar.
#[allow(async_fn_in_trait)]
pub trait MigrationBackend {
    /// Opaque backend-owned session state. The generic executor only
    /// round-trips this value from [`snapshot_session`](Self::snapshot_session)
    /// to [`restore_session`](Self::restore_session).
    type SessionSnapshot;

    /// The SQL dialect this backend targets. Drives the dialect-boundary rejects
    /// in the generic body (e.g. `transaction:false` on SQLite, design §2.3/L3).
    fn dialect(&self) -> SqlDialect;

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

    /// Acquire the project apply-serialization lock (PG `pg_advisory_lock`).
    async fn acquire_project_lock(&self, project_id: &str) -> Result<(), ApplyError>;

    /// Release the project apply-serialization lock (PG `pg_advisory_unlock`).
    async fn release_project_lock(&self, project_id: &str) -> Result<(), ApplyError>;

    /// Snapshot the session settings the apply will override, for restore on exit.
    async fn snapshot_session(&self) -> Result<Self::SessionSnapshot, ApplyError>;

    /// Restore the settings captured by [`snapshot_session`](Self::snapshot_session).
    /// Includes the leading unconditional role reset where the backend has roles.
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
    ///   marker, with idempotent crash recovery when `had_inflight`.
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

    /// Roll back ONE migration transactionally (design §5): `BEGIN; SET LOCAL …;
    /// SET LOCAL ROLE migrator; <down>; RESET ROLE; INSERT rolled_back; COMMIT`.
    /// `down` + the `rolled_back` append commit atomically.
    async fn rollback_one_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError>;

    // -- parse-time validation ----------------------------------------------

    /// Validate that a **non-transactional** migration's `up` is idempotent
    /// (re-runnable by crash recovery). PG parses with `pg_query` and enforces
    /// `IF NOT EXISTS` on `CREATE INDEX CONCURRENTLY` / `ALTER TYPE … ADD VALUE`
    /// and forbids bare DML; a SQLite backend rejects `transaction:false`
    /// outright (no non-txn DDL exists on SQLite, design §2.3/L3).
    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError>;

    // -- journal row I/O (dialect-neutral owned rows) -----------------------

    /// Bootstrap the journal + immutability constructs (idempotent).
    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError>;

    /// The net-applied + lone-`started` journal entries (the drift/pending input).
    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError>;

    /// The versions covered by a net-applied squash (the supersession net-state).
    async fn superseded_versions(&self, cfg: &ExecutorConfig)
        -> Result<Vec<String>, JournalError>;

    /// The latest journaled `completed` checksum per identity — the repeatable
    /// re-run oracle.
    async fn latest_completed_checksums(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<std::collections::HashMap<String, String>, JournalError>;

    /// The cross-deploy pending-contract capability (§2.0.3).
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
    async fn snapshot_schema(&self, cfg: &ExecutorConfig)
        -> Result<SchemaSnapshot, DriftError>;

    // -- preconditions (DB-coupled; full seam is post-P1) -------------------

    /// Evaluate a migration's preconditions read-only under the apply lock.
    /// Behind the trait so the generic body never holds a concrete connection;
    /// the PG impl delegates to [`crate::apply::precondition::evaluate`].
    async fn evaluate_preconditions(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<crate::apply::executor::PreconditionVerdict, ApplyError>;

    // -- squash (DB-coupled supersession journal write) ---------------------

    /// Journal a **squash** as a `completed` `kind='squash'` event WITHOUT running
    /// its `up`, plus the `S → v_i` supersession edges — the dialect-coupled write
    /// behind the generic [`crate::squash::squash`] (multi-engine abstraction C3).
    ///
    /// Called only after the generic body has verified, under the project lock,
    /// that ALL of `supersedes` are net-applied (the existing-DB squash path). The
    /// PG impl delegates to [`crate::journal::record_baseline`] with
    /// `kind='squash'`; a SQLite impl writes the same row+edges atomically through
    /// its actor. The connection / `pg_advisory_lock` never cross this surface.
    ///
    /// # Errors
    /// [`ApplyError::Db`] (PG) or [`ApplyError::Backend`] (non-PG) on a write
    /// failure; the dialect-neutral [`crate::executor::ApplyError`] is what crosses
    /// the trait.
    async fn record_squash(
        &self,
        cfg: &ExecutorConfig,
        squash_migration: &Migration,
        applied_by: &str,
        supersedes: &[&str],
    ) -> Result<(), ApplyError>;

    // -- declarative-only structured ops (P6a) ------------------------------

    /// Apply ONE structured SQLite 12-step table REBUILD atomically with
    /// confinement + journal it (design §2.4), the dialect-coupled drive behind the
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
    /// `SqliteRebuild` (its differ uses native `ALTER` / expand-contract), so a rebuild
    /// reaching the PG backend is a routing bug, surfaced as a clear error rather than
    /// a silent pass.
    ///
    /// **PR9b — per-version approval scope (executor-layer defense in depth).** A
    /// rebuild on a populated table is destructive (drop + recreate + copy), so when
    /// `m.flags.destructive` (always true for a `SqliteRebuild` by construction,
    /// [`crate::declarative`]) the `scope` must admit `m.version` — mirroring
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
        spec: &SqliteRebuildSpec,
        m: &Migration,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<(), ApplyError>;

    /// Drive a single **batched data backfill** step (`op.*` DSL §2.0,
    /// [`PlanStep::Backfill`](crate::render::step::PlanStep::Backfill)) through the
    /// dialect seam, journaling the spec's marker version. On Postgres this
    /// delegates to the existing [`run_backfill`](crate::backfill::run_backfill)
    /// (writable-CTE windowed `UPDATE`). On SQLite the batched-backfill executor
    /// is **net-new and committed for PR6b** — until it lands, the SQLite arm
    /// fails closed with a clear [`ApplyError::Backend`] rather than silently
    /// skipping the data transform (a SQLite-targeted batched backfill is a hard
    /// error, never a silent mis-apply, §6/§10 PR6a-PR6b).
    ///
    /// `lock_mode` mirrors the per-`Migration` apply: under
    /// [`LockMode::AlreadyHeld`](crate::executor::LockMode) the caller already
    /// holds the project lock for the whole deploy (the PG per-batch
    /// `pg_advisory_xact_lock` inside the backfill is re-entrant under it).
    ///
    /// # Errors
    /// [`ApplyError::Backend`] on a backfill failure or the SQLite-unsupported
    /// arm (PR0/PR6a); the failure is resumable from the last committed cursor on
    /// PG.
    ///
    /// **PR9b — per-version approval scope (executor-layer defense in depth).** A
    /// standalone batched backfill is a NON-destructive data transform
    /// ([`PlanStep::approval_scope_version`](crate::render::step::PlanStep::approval_scope_version)
    /// returns `None` for `Backfill`), so the per-version scope never refuses it — the
    /// `scope` is threaded for seam-signature symmetry with the destructive seam
    /// methods (and forward-proofing) and consulted only if a backfill ever becomes
    /// scope-gated. The data-mutating EXPAND backfill rides
    /// [`OnlineSchemaChange::run_online`](crate::expand_contract::OnlineSchemaChange::run_online),
    /// NOT this method.
    async fn run_backfill_step(
        &self,
        cfg: &ExecutorConfig,
        spec: &BackfillSpec,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        lock_mode: crate::apply::executor::LockMode,
    ) -> Result<crate::apply::executor::ApplyOutcome, ApplyError>;

    /// Drive a single **parameterized DML** step (`op.*` DSL §2.3.2,
    /// [`PlanStep::Dml`](crate::render::step::PlanStep::Dml)) through the dialect seam.
    /// The `template` is executed with `binds` bound NATIVELY (`$n` on PG, `?n`
    /// on SQLite) — never string-interpolated, so a bind value can never alter
    /// statement structure. The step is journaled under `version` (its
    /// sub-version), so a re-run is a net-applied-skip (idempotent).
    ///
    /// PR0 ships the PG executor + a trusted constructor for tests; the creator
    /// DML *assembler* that produces `(template, binds)` from `op.insert`/
    /// `op.update` is net-new in PR6a. The SQLite one-shot DML executor is also
    /// PR6a (the shared SQLite-DML module); until it lands the SQLite arm fails
    /// closed.
    ///
    /// `destructive` carries the step's data-loss flag (a `delete`). The
    /// implementation re-runs the approval gate as **defense in depth**: a
    /// destructive DML under any approval other than [`Approval::Approved`] is
    /// refused with [`ApplyError::ApprovalRequired`] BEFORE the template executes,
    /// mirroring the per-`Migration` gate in
    /// [`apply_with_lock_backend`](crate::executor::apply_with_lock_backend). The
    /// engine's [`apply_plan`](crate::engine::MigrationEngine::apply_plan) gate runs
    /// first; this is the independent executor-layer check so a direct seam caller
    /// cannot bypass it.
    ///
    /// **PR9b — per-version approval scope (executor-layer defense in depth).** On top
    /// of the coarse approval gate, a destructive DML runs ONLY if `scope` admits its
    /// `version` — mirroring
    /// [`PlanStep::approval_scope_version`](crate::render::step::PlanStep::approval_scope_version)'s
    /// rule for `Dml`. So a direct seam caller driving `run_dml_step` with blanket
    /// [`Approval::Approved`] but an [`ApprovalScope::Versions`] set that omits this
    /// `version` is refused with [`ApplyError::ApprovalNotScoped`] BEFORE the template
    /// executes — the executor-layer mirror of the engine's per-version scope gate.
    ///
    /// `owner_app` is the declaring app's identity — it is folded into the journal
    /// [`ChecksumInput`](crate::migration::ChecksumInput) so two DML steps with an
    /// identical `(template, binds)` authored by DIFFERENT apps hash to DIFFERENT
    /// journal checksums (correct multi-tenant journal identity/attribution for
    /// PR6a's creator-DML assembler).
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
        version: &crate::model::migration::MigrationId,
        name: &str,
        template: &str,
        binds: &[BindValue],
        destructive: bool,
        owner_app: &str,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        lock_mode: crate::apply::executor::LockMode,
    ) -> Result<bool, ApplyError>;

    /// The **online schema-change capability** (multi-engine abstraction L1/L2/H1).
    ///
    /// `Some(&dyn OnlineSchemaChange)` for an engine that drives zero-downtime
    /// online operations (Postgres — expand-contract rename via dual-write trigger
    /// plus paged backfill; the [`PgOnline`](crate::expand_contract::PgOnline) impl
    /// owns its `Client` **internally**, so the connection NEVER appears on this
    /// neutral trait surface). `None` for an engine with no online path (SQLite,
    /// where every existing-table change, rename included, is routed to a
    /// [`rebuild_one`](Self::rebuild_one), so its declarative `renames` set is
    /// structurally EMPTY).
    ///
    /// This REPLACES the old `expand_conn() -> Option<&compio_postgres::Client>`
    /// escape hatch: the generic declarative apply path branches on
    /// `online().is_some()`, never holding a concrete connection, and a backend
    /// with no online capability MUST receive an empty `renames` (asserted at the
    /// call site — a non-empty rename set with `online() == None` is a routing bug).
    fn online(&self) -> Option<&dyn crate::ops::expand_contract::OnlineSchemaChange>;

    /// The **shadow dry-run capability** (multi-engine abstraction C3).
    ///
    /// `Some(&dyn ShadowDryRun)` for an engine that can preview a migration batch
    /// against a throwaway shadow clone before the real apply (Postgres — the
    /// [`PgShadow`](crate::shadow::PgShadow) impl owns its admin `Client`
    /// **internally**, so the connection NEVER appears on this neutral trait
    /// surface). `None` for an engine with no shadow path.
    ///
    /// This REPLACES the old `MigrationEngine::dry_run(admin_conn: &Client, …)` /
    /// `PostgresBackend::conn() -> &Client` escape hatch: the engine's
    /// [`dry_run`](crate::engine::MigrationEngine::dry_run) /
    /// [`dry_run_declarative`](crate::engine::MigrationEngine::dry_run_declarative)
    /// branch on `shadow().is_some()`, never holding a concrete connection, and a
    /// backend with no shadow capability surfaces a clear
    /// [`DryRunError::ShadowUnsupported`](crate::shadow::DryRunError::ShadowUnsupported)
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
    fn shadow(&self) -> Option<&dyn crate::ops::shadow::ShadowDryRun>;

    // -- baseline / adoption ------------------------------------------------

    /// Adopt the LIVE schema as the project's **baseline** (design §5 / H3,
    /// multi-engine abstraction L5) — a `kind='baseline'`, `completed` journal
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
    ///   failure, mapped onto the dialect-neutral arm.
    async fn baseline_one(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<BaselineOutcome, BaselineError>;
}
