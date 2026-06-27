//! `zeroship-migrate` — zeroship's versioned DB migration engine for **creator
//! project databases** (design `docs/proposals/2026-06-16-db-migration-engine-design.md`).
//!
//! This crate implements the **security core** + the **migration unit** (the
//! migration data types §2.1 and the parse-time SQL security guard §1.4
//! deny-list / §1.5 cross-schema confinement), and the **Postgres executor**
//! (§2.3): the append-only journal ([`apply::journal`](crate::apply::journal)),
//! the project advisory lock, and the apply flow
//! ([`apply::executor::apply`](crate::apply::executor::apply)) — transactional + two-phase
//! non-transactional with idempotent recovery, the guard wired in front of
//! every `up`, and a drift/tamper checksum check — the least-privilege
//! `migrator` role ([`apply::role`](crate::apply::role)), and the **public
//! authoring pipeline + engine API** ([`plan::author`](crate::plan::author) +
//! [`engine`]).
//!
//! # The pipeline
//!
//! ```text
//! author -> plan (lint) -> gate (approval) -> executor::apply (guard + role)
//! ```
//!
//! 1. an **author** ([`MigrationAuthor`]) produces versioned [`Migration`]s.
//!    [`DeterministicAuthor`] handles the trivial additive set (create table,
//!    add column, create index) with no AI; [`RawSqlAuthor`] is the **AI-author
//!    hook** — the AI/builder generates complex migrations (renames, type
//!    changes, backfills, expand-contract) *externally* and this engine
//!    validates + executes them. The engine never calls an LLM.
//! 2. [`MigrationEngine::plan`] runs the [`guard::SqlGuard`] read-only and
//!    returns a [`MigrationPlan`] (the dry-run/preview): destructive flags,
//!    approval requirement, and guard *denials*.
//! 3. [`MigrationEngine::apply`] is the **gate** ([`Approval`]): it refuses a
//!    denied plan, refuses a destructive plan without approval, and otherwise
//!    delegates to [`apply::executor::apply`](crate::apply::executor::apply) — which **independently re-runs the
//!    guard and the least-privilege `migrator` role** (defense in depth: the
//!    engine gate is an additional check, not a replacement for lines 1 & 2).
//!
//! # Security stance (§1)
//!
//! Migrations are **privileged arbitrary-SQL** authored by **untrusted**
//! creators *and* a **prompt-injectable AI**. The threat surface is
//! cross-tenant access, privilege escalation, Postgres host-escape / RCE,
//! filesystem + network reach, and data loss.
//!
//! Defense is in depth:
//!
//! - **Line 1 — this guard ([`guard::SqlGuard`]).** Every statement is parsed
//!   with the *real* Postgres parser (`pg_query`/`libpg_query` — chosen over a
//!   pure-Rust parser precisely so a deny-list cannot be bypassed by exotic
//!   syntax it would misparse) and checked against a hard deny-list. Dangerous
//!   constructs nested inside `DO $$…$$` blocks and function bodies are
//!   inspected too, not just top-level statements. Unparseable input is
//!   denied. The guard **denies** RCE / priv-esc / cross-tenant / file /
//!   network, and only **flags** data loss (`DROP`/`TRUNCATE`/lossy type
//!   change) — the apply gate (a later plan) decides on destructive ops.
//! - **Line 2 — the least-privilege `migrator` role** (a later plan). The DB
//!   itself rejects the same ops even if SQL somehow slips past parse.
//!
//! The guard runs **out-of-band at deploy time** (not on the request hot path),
//! so it is plain synchronous logic — no tokio/compio — and exhaustively
//! unit-testable without a database (`tests/guard_security.rs`).

pub mod analysis;
pub mod apply;
pub mod approval;
pub mod command;
pub mod conn;
pub mod engine;
#[doc(hidden)]
pub mod fault;
pub mod guard;
pub mod model;
pub mod ops;
pub mod plan;
pub mod render;

pub use analysis::{analyze, classify};

// ---------------------------------------------------------------------------
// Public API surface — re-exports (later plans depend on these names).
// ---------------------------------------------------------------------------

pub use analysis::analyze::{analyze, analyze_migration, Advisory, Severity};
pub use approval::{Approval, ApprovalScope};
pub use apply::backend::{
    BackfillError, BackfillOutcome, CrossDeployObligations, DryRunError, DryRunReport,
    MigrationBackend, MigrationResult, OnlineSchemaChange, PgSessionSnapshot, PostgresBackend,
    SeedError, ShadowConfig, ShadowDryRun,
};
pub use apply::backend::sqlite::{RebuildError, SqliteActorError, SqliteBackend};
pub use apply::backend::postgres::online::PgOnline;
pub use apply::backend::postgres::shadow::PgShadow;
pub use apply::baseline::{BaselineError, BaselineOutcome};
pub use apply::backend::postgres::backfill::{
    backfill_progress, ensure_backfill_progress, list_backfills, run_backfill,
    run_backfill_bounded, BackfillProgress,
};
pub use plan::author::{
    AuthorError, AuthorRequest, Column, DeterministicAuthor, MigrationAuthor, RawSqlAuthor,
};
pub use analysis::classify::{
    classify, relations_touched, DdlKind, OwnershipNeed, ParseError, StatementClass,
    TouchedRelation,
};
pub use render::declarative::{
    desired_snapshot, desired_snapshot_for_dialect, dsl_to_pg_data_type, sqlite_canonical_type,
    CollectionDescriptor, DeclarativeAuthor, DeclarativeError, DeclarativePlan, DesiredSchema,
    FieldDescriptor, IndexDescriptor, RenameHint, SqliteRebuild,
};
pub use engine::{
    recognizes_contract_apply, DeclarativeApplyError, DeclarativeDeployOutcome,
    DeclarativeDeployPlan, EngineError, MigrationEngine, MigrationPlan, OnlineError,
    PlannedMigration, RollbackEngineError,
};
pub use render::expand_contract::{
    ExpandContractAuthor, ExpandContractError, ExpandContractPlan, OnlineIntent,
};
pub use conn::{connect, ConnectError, ExecutorConfig, PgConfinement};
pub use apply::drift::{
    check_checksum_drift, diff_snapshots, snapshot_schema, AlteredObject, ChecksumDrift,
    ChecksumDriftReport, DriftError, DriftReport, OrphanJournal, StructuralDrift,
};
pub use apply::executor::{
    apply, rollback, ApplyError, ApplyOutcome, BackendError, LockMode, PreconditionVerdict,
    RollbackError, RollbackOptions, RollbackOutcome, RollbackRequest, RollbackTarget,
};
// **Migration-first P1** — the OFFLINE ops→snapshot fold (the keystone). Pure, no
// DB: replay an ordered `Op` list into the EXISTING `SchemaSnapshot` (drift.rs),
// the offline companion of `snapshot_schema`. Later phases (`gen-types`) emit the
// `env.db` types + runtime descriptor from this. See `fold.rs`.
pub use render::fold::{
    descriptors_to_create_ops, fold_ops, fold_to_field_defs, recover_check_facet, FoldError,
    ProduceError, RecoveredCheck,
};
pub use guard::{
    flags_for, guard_for, GuardConfig, GuardError, GuardOutcome, GuardReport, MigrationGuard,
    PgGuard, SqlGuard, SqliteDescriptorGuard,
};
pub use model::policy::{SchemaScope, TrustProfile};
// The deploy-target dialect (§2.4.1) — re-exported so the control-plane deploy
// path can thread it into `IrAuthor::new` without depending on `zeroship-schema`.
pub use zeroship_schema::query::SqlDialect;
pub use apply::journal::{
    applied, applied_count, ensure_journal, history as journal_history,
    latest_completed_checksums, net_rolled_back, outstanding_pending_contracts,
    record_baseline, record_completed, record_rolled_back, record_started,
    resolve_pending_contract, superseded_versions, AppliedEntry, HistoryEvent,
    HistoryKind, JournalError, JournaledKind, PendingContract, PendingContractRecord,
    PendingState, Phase, Resolution, RolledBackEntry,
};
// The §8.8 structured pending-contract interlock payloads (§2.0.3 / §2.0.4).
pub use plan::pending::{
    ActionPayload, DependencyPendingContract, OrphanedPendingContract, PendingContractRefusal,
    CODE_DEPENDENCY_PENDING_CONTRACT, CODE_ORPHANED_PENDING_CONTRACT,
    CODE_TABLE_HAS_PENDING_CONTRACT,
};
pub use plan::loader::{
    load_dir, load_dir_migrations, new_dbmate_migration, repeatable_id_for_name, LoaderError,
    PLATFORM_OWNER_APP,
};
pub use ops::squash::{squash, SquashError, SquashOutcome};
pub use ops::status::{
    history, status, BlockedPlan, MigrationStatus, PendingContractStatus, StatusError,
};
pub use ops::submit::{
    submit_migration, Submission, SubmissionOutcome, SubmitError,
};
pub use plan::manifest::{
    compute_manifest, verify_manifest, ManifestError, ManifestHash, MismatchKind,
};
pub use model::migration::{
    migration_id_for_version, Checksum, ChecksumInput, IdError, Migration, MigrationFlags,
    MigrationId, OnlinePhase, MIGRATION_PREFIX,
};
pub use model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, GeneratedColumnSnapshot, IndexSnapshot, NamedTypeSnapshot,
    SchemaSnapshot, SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
// The `op.*` portable IR (§2.1/§2.3/§2.5): the migration document, the closed
// `Op` enum, the constrained numeric scalar, and the canonical op-list the
// `Checksum::of_ir` front door folds. There is NO `Raw`/`RawDown` (property A);
// every transform/predicate is the closed [`expr::Expr`] AST.
pub use model::ir::{
    CanonicalOpList, ColType, ColumnOrExpr, ExclusionElement, ExclusionMethod,
    ExclusionOperator, GeneratedCol, IdentityCol, IndexMethod, IrBatch, IrClassification,
    IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrFlagsOverride, IrIndex, IrMask,
    IrMaskKind, IrScalar, IrVersionError, MigrationIr, Op, RefAction, SafeU64,
    SequenceOwnedBy, SynthDefaultFn, VectorMetric, CURRENT_IR_VERSION, EXPR_INVALID_NUMERIC,
};
// The fail-closed `.ir.json` load gate (§5.2/§5.3/§8.6): deserialize →
// `ir_version` → `validate_ir` → server-stamped ownership → advisory checksum-hint
// compare. The loader's IR branch ([`render::lower::IrAuthor::load_and_lower`]) runs
// this gate and then lowers the validated, owned IR to migrations (§7.2).
pub use model::load::{
    enforce_ir_ownership, hint_domain_uncomputable_field, load_ir_document,
    recompute_hint_domain_checksum, IrLoadError,
};
// PR7 online-rename go-live (SQLite leg): the deploy/dev entry point that applies a
// bundle's `.ir.json` set against a SQLite backend, building the SQLite-dialect
// LiveSchema from the app's descriptor set so an IR `renameColumn` lowers + applies
// via `rebuild_one` end-to-end.
pub use command::ir_apply::{
    apply_bundle_ir_sqlite, apply_bundle_ir_sqlite_catalog, SqliteIrApplyError,
    SqliteIrApplyOutcome,
};
// The IR-path DDL Lower phase (§6/§6.4/§6.5): compiles a validated, ownership-
// checked `MigrationIr` to migrations, reusing the SHARED snapshot-builder +
// declarative render seam so its SQL is byte-identical to the differ's path.
pub use render::lower::{
    FragmentGuardDenied, GuardedFragment, IrAuthor, IrGuardedLowerError, IrLowerError, LiveSchema,
    LoadAndLowerError, LoadAndLowerGuardedError, LoweredArtifact,
};
// The closed expression AST (§3.3.1) the IR's transform/predicate positions
// carry. Constructed in JS, serialized as data, NEVER parsed from text.
pub use model::expr::{BinaryOp, CaseBranch, CastTarget, Expr, ScalarFn, SynthFn, UnaryOp};
// The STRUCTURAL expression-AST validator + the structured-error envelope
// (§3.3.1.1 / §8.8). No parser, no fuzzer — a pure allow-list walk.
pub use model::validate::{
    validate_expr, validate_ir, validate_ir_resolved, validate_op, validate_op_resolved,
    AuthoringError, Dialect as ValidatorDialect, TargetScope, UnsupportedKind,
    CODE_COLUMN_FACET_CONFLICT, CODE_DIALECT_SCOPE_PGONLY, CODE_EXPR_NOT_PORTABLE,
    CODE_OP_OUTSIDE_RECORDER, CODE_UNSUPPORTED, SPLIT_PART_MAX_N,
};
// The `op.*` DSL plan model (§2.0). Distinct from the dry-run `MigrationPlan`
// (re-exported from `engine`, unchanged): these are the net-new ordered
// EXECUTION artifact + its steps. `+AppliedPlan, +PlanStep, +RenameStep` added;
// `MigrationPlan` kept.
pub use model::backfill::BackfillSpec;
pub use model::probe::{ExpectColumn, GuardDir, GuardProbe};
pub use render::plan::{AppliedPlan, NotSingleStep, SqliteRebuildSpec};
pub use render::step::{tables_touched_by, BindValue, DialectScope, PlanStep, RenameStep};
// PR14 — the OFFLINE `--sql` plan preview (operator go-live review). A pure,
// DB-free surfacing/formatting layer over the SQL `IrAuthor::lower_*` already
// lowers; DB-state-dependent ops are labeled `-- [runtime-resolved]`, never
// fabricated.
pub use render::sql_preview::{
    render_ir_json_sql, render_plan_sql, render_set_sql, PreviewOpts, RUNTIME_RESOLVED,
};
pub use apply::precondition::{evaluate as evaluate_precondition, PreconditionError};
pub use model::precondition::{CmpOp, OnUnmet, Precondition, PreconditionCheck};
pub use apply::role::{deprovision_migrator, migrator_role_name, provision_migrator, RoleError};
pub use apply::backend::postgres::shadow::{
    dry_run, dry_run_declarative, dry_run_incremental, sweep_leaked_shadows,
};
#[doc(hidden)]
pub use apply::backend::postgres::shadow::arm_panic_after_provision;
