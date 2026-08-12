//! `zero-migrate` — a versioned DB migration engine for **creator
//! project databases**. The shipped design docs are under `docs/`; start at
//! `docs/architecture.md`.
//!
//! The engine is runtime-free and V8-free. Authoring (the JS DSL to op-IR
//! envelope) and live Postgres/MySQL execution run in the Node host and reach
//! the engine through the `zero-migrate-node` napi bridge; the Postgres apply
//! path is the driver-neutral [`SqlSession`] seam. In-process SQLite is the one
//! backend the engine drives directly.
//!
//! This crate implements the **security core** + the **migration unit** (the
//! migration data types and the parse-time SQL security guard
//! deny-list / cross-schema confinement), and the **Postgres executor**:
//! the append-only journal ([`apply::journal`]),
//! the project advisory lock, and the apply flow
//! ([`apply::executor::apply`]) — transactional + two-phase
//! non-transactional with idempotent recovery, the guard wired in front of
//! every `up`, and a drift/tamper checksum check — the least-privilege
//! `migrator` role ([`apply::role`]), and the **public
//! authoring pipeline + engine API** ([`plan::author`] +
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
//!    delegates to [`apply::executor::apply`] — which **independently re-runs the
//!    guard and the least-privilege `migrator` role** (defense in depth: the
//!    engine gate is an additional check, not a replacement for lines 1 & 2).
//!
//! # Security stance
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
//!   change) — the apply gate decides on destructive ops.
//! - **Line 2 — the least-privilege `migrator` role.** The DB
//!   itself rejects the same ops even if SQL somehow slips past parse.
//!
//! The guard runs **out-of-band at deploy time** (not on the request hot path),
//! so it is plain synchronous logic — no async runtime — and exhaustively
//! unit-testable without a database (`tests/guard_security.rs`).

// The SQL security layer (`analysis` + `guard`) lives in the `zero-migrate-guard`
// crate. Re-export its modules under their historical
// `crate::{analysis,guard}` paths so the engine's ~dozens of
// `crate::guard::…` / `crate::analysis::…` references keep resolving unchanged.
pub use zero_migrate_guard::{analysis, guard};
pub mod apply;
pub mod approval;
pub mod conn;
pub mod db_url;
pub mod engine;
#[doc(hidden)]
pub mod fault;
// The typed-id (base62/UUIDv7) machinery lives in the `zero-migrate-ir` leaf crate;
// re-export it under its historical `crate::id` path.
pub use zero_migrate_ir::id;
// The deploy-bundle migration-file record + content-addressed hash, vendored
// byte-identically from the upstream bundle layer so the build
// front-end emits bundle entries without an upstream bundle-layer normal-graph dep.
pub mod manifest_entry;
pub mod model;
// The reviewed-allowlist net-policy security types, vendored byte-identically
// from the upstream core so the recorder-sandbox / MySQL
// JS-driver `NetPolicy` names no upstream-core type.
pub mod net_policy;
pub mod ops;
pub mod plan;
pub mod render;
// The dialect-neutral network driver seam (`SqlSession`) — the ONE injected
// runtime dependency the network-dialect backends (`PostgresBackend`, and the
// forthcoming `MysqlBackend`) are generic over. SQLite does NOT ride it (it is an
// in-process rusqlite actor). Gated on `pg_seam` (its only current implementor is
// the host PG adapter, lit by the `host-pg` feature); a `--no-default-features`
// build keeps a lean core.
#[cfg(pg_seam)]
pub mod driver;
// The schema-authority core (DDL builders, diff classifier, sentinel codec,
// schema-shape descriptors). The data-plane query language that used to ride
// alongside had zero engine callers and was deleted; only the write/diff/describe
// layer the engine uses survives here.
pub mod schema;

// The guard behaviour-lock suite — an in-crate test module so it can drive
// the guard through the engine's `render::lower` / `conn` internals.
#[cfg(test)]
mod guard_vendor_lower_tests;
#[cfg(test)]
pub(crate) mod test_fixtures;

pub use analysis::{analyze, classify};

// ---------------------------------------------------------------------------
// Public API surface — re-exports.
// ---------------------------------------------------------------------------

pub use analysis::analyze::{analyze, analyze_migration, Advisory, Severity};
pub use approval::{Approval, ApprovalScope};
// V8-free, driver-neutral re-exports. Name no host-driver type and back the
// SQLite path too — ungated.
pub use apply::backend::{
    BackfillError, BackfillOutcome, CrossDeployObligations, DryRunError, DryRunReport,
    MigrationBackend, MigrationResult, OnlineSchemaChange, SeedError, ShadowConfig, ShadowDryRun,
};
// PG-seam re-exports (consumer-based gate: `PgSessionSnapshot` is a pure-`String`
// struct, but its only consumers are the PG session leaves). Behind the PG seam
// (`host-pg`) — the generic `PostgresBackend<D>` compiles there.
#[cfg(pg_seam)]
pub use apply::backend::{PgSessionSnapshot, PostgresBackend};
// The driver-neutral `SqlSession` seam types (the engine-root `crate::driver`
// module). Public so a host (napi) driver can construct return values / binds,
// and so error consumers read the neutral `DbError` (SQLSTATE in `.sqlstate`). On
// the whole PG seam — the addon (`host-pg`) is the primary consumer of these
// neutral types. MySQL will ride the same seam; SQLite does NOT (in-process
// rusqlite).
pub use analysis::classify::{
    classify, drop_index_targets, relations_touched, DdlKind, DropIndexTarget, OwnershipNeed,
    ParseError, StatementClass, TouchedRelation,
};
pub use apply::backend::sqlite::{RebuildError, SqliteActorError, SqliteBackend};
pub use apply::baseline::{BaselineError, BaselineOutcome};
pub use apply::drift::{
    diff_snapshots, diff_snapshots_with_index_aliases, AlteredObject, ChecksumDrift,
    ChecksumDriftReport, DriftError, DriftReport, OrphanJournal, StructuralDrift,
};
pub use conn::{ConnectError, ExecutorConfig, PgConfinement};
#[cfg(pg_seam)]
pub use driver::{Bind, ColIndex, DbError, FromValue, Row, SqlSession, Value};
pub use engine::{
    recognizes_contract_apply, AggregateOutcome, DeclarativeApplyError, DeclarativeDeployOutcome,
    DeclarativeDeployPlan, EngineError, MigrationEngine, MigrationPlan, OnlineError,
    PlannedMigration, RollbackEngineError,
};
pub use plan::author::{
    AuthorError, AuthorRequest, Column, DeterministicAuthor, MigrationAuthor, RawSqlAuthor,
};
pub use render::declarative::{
    desired_snapshot, desired_snapshot_for_dialect, dsl_to_pg_data_type, sqlite_canonical_type,
    AcceptedIndexAlias, CollectionDescriptor, DeclarativeAuthor, DeclarativeError, DeclarativePlan,
    DesiredSchema, FieldDescriptor, IndexDescriptor, RenameHint, SqliteRebuild,
};
pub use render::expand_contract::{
    ExpandContractAuthor, ExpandContractError, ExpandContractPlan, OnlineIntent,
};
// `check_checksum_drift` reads the journal over a `&D: SqlSession`; `snapshot_schema`
// introspects `pg_catalog` — both generic over the seam (SQLite has its own peers).
// On the whole PG seam.
#[cfg(pg_seam)]
pub use apply::drift::{check_checksum_drift, snapshot_schema};
pub use apply::executor::{
    ApplyError, ApplyOutcome, BackendError, LockMode, PreconditionVerdict, RollbackError,
    RollbackOptions, RollbackOutcome, RollbackRequest, RollbackTarget,
};
// The rollback verb, and the planner that decides every refusal before it runs.
// Generic over `MigrationBackend`, so unlike `apply` below it is not PG-gated.
pub use apply::executor::{
    plan_rollback, rollback, rollback_with_lock, AppliedRecord, RollbackPlan,
};
// `apply` is generic over the `SqlSession` seam — on the whole PG
// seam. `rollback` is still `&Client`-typed (out of v1 scope) — PG-only.
#[cfg(pg_seam)]
pub use apply::executor::apply;
// The OFFLINE ops→snapshot fold. Pure, no
// DB: replay an ordered `Op` list into the EXISTING `SchemaSnapshot` (drift.rs),
// the offline companion of `snapshot_schema`. The type-generation path emits the
// `env.db` types + runtime descriptor from this. See `fold.rs`.
pub use guard::{
    flags_for, guard_for, GuardConfig, GuardError, GuardMode, GuardOutcome, GuardReport,
    MigrationGuard, PgGuard, SqlGuard, SqliteDescriptorGuard,
};
pub use model::policy::{DestructiveOps, SchemaScope, TrustProfile};
// The policy PDP seal primitives: an HMAC over a composed `EffectivePolicy`, bound
// to the registry digest, the scope-matcher semantics, and the charter revision, so
// a seal minted under any of them fails to verify under another.
pub use model::table_shape::{
    effective_policy_from_charter_layers, effective_policy_from_charter_toml,
    resolve_create_table_policy, ResolvedInject, TableShapeError,
};
pub use zero_migrate_policy::{seal, SealError, SealedPolicy};
// The composed policy-decision point the injection + guard share. Re-exported at
// the crate root so the napi addon (`gen_artifacts_*`, the schema-emit path) can
// name it without reaching into the `zero-migrate-policy` crate directly.
pub use render::fold::{
    descriptors_to_create_ops, fold_ops, fold_ops_onto, fold_to_field_defs,
    history_carries_dialectal_ops, recover_check_facet, FoldError, ProduceError, RecoveredCheck,
};
pub use zero_migrate_policy::EffectivePolicy;
// The `gen-types` schema-artifact emitter: fold a schema source (op.* migrations or
// a declared `CollectionDescriptor` set) into the two co-emitted projections
// (`schema.runtime.json` v1 descriptor + generated `env.db.ts`), plus the in-memory
// `--check` drift gate. Both sources route through the SAME renderer, so output is
// byte-identical for equivalent schemas.
pub use render::gen_types::{
    check_artifacts, diff_artifacts, render_artifacts, render_artifacts_from_descriptors,
    CheckDiff, GenTypesError, GeneratedArtifacts, DEFAULT_PROJECT_SCHEMA, ENV_DTS_FILE,
    RUNTIME_DESCRIPTOR_FILE,
};
// The deploy-target dialect — re-exported so an embedding host's deploy
// path can thread it into `IrAuthor::new`.
pub use schema::query::SqlDialect;
// Dialect-neutral journal types (the SQLite path constructs/imports these too).
pub use apply::journal::{
    AppliedEntry, HistoryEvent, HistoryKind, JournalError, JournaledKind, PendingContract,
    PendingContractRecord, PendingState, Phase, Resolution, RolledBackEntry,
};
// The PG journal free functions (each takes a `&D: SqlSession` connection). The
// SQLite backend has its own `sqlite/journal_sql.rs` peers. On the whole PG seam.
#[cfg(pg_seam)]
pub use apply::journal::{
    applied, applied_count, ensure_journal, history as journal_history, latest_completed_checksums,
    net_rolled_back, outstanding_pending_contracts, record_baseline, record_completed,
    record_rolled_back, record_started, resolve_pending_contract, superseded_versions,
};
// The structured pending-contract interlock payloads.
pub use ops::squash::{squash, SquashError, SquashOutcome};
pub use ops::status::{
    AppliedPlanStatus, BlockedPlan, MigrationStatus, PendingContractStatus, PlanStatusManifest,
    PlanStatusManifestStep, PlanStatusStep, PlanStatusStepKind, PlanStatusStepState,
    ReconciledPlan, ReconciledPlanState, ResolvedPendingContract, StatusError, StatusSnapshot,
};
// The non-blocking project-lock acquisition a read-only verb reports contention
// with, and the holder detail its operator message names.
pub use apply::backend::{ProjectLockAcquisition, ProjectLockHolder};
pub use plan::pending::{
    ActionPayload, DependencyPendingContract, OrphanedPendingContract, PendingContractRefusal,
    CODE_DEPENDENCY_PENDING_CONTRACT, CODE_ORPHANED_PENDING_CONTRACT,
    CODE_TABLE_HAS_PENDING_CONTRACT,
};
// `history` / `status` are generic over the `SqlSession` seam —
// on the whole PG seam so a host driver can drive the pending-migrations flow.
#[cfg(pg_seam)]
pub use ops::status::{history, status};
// The confined submit path is PG-only; gated with `mod ops::submit`.
pub use model::migration::{
    migration_id_for_version, Checksum, ChecksumInput, IdError, Migration, MigrationFlags,
    MigrationId, OnlinePhase, MIGRATION_PREFIX,
};
pub use model::snapshot::{
    ColumnCollationSnapshot, ColumnSnapshot, ConstraintSnapshot, ExtensionSnapshot, FunctionKey,
    FunctionSnapshot, GeneratedColumnSnapshot, IndexElementSnapshot, IndexSnapshot,
    MysqlTextStorageSnapshot, NamedTypeSnapshot, PartitionSnapshot, RoleSnapshot,
    SchemaObjectSnapshot, SchemaSnapshot, SequenceDataTypeSnapshot, SequenceSnapshot,
    TableSnapshot, TriggerKey, TriggerSnapshot, ViewSnapshot,
};
pub use plan::manifest::{
    compute_manifest, verify_manifest, ManifestError, ManifestHash, MismatchKind,
};
// The `op.*` portable IR: the migration document, the closed
// `Op` enum, the constrained numeric scalar, and the canonical op-list the
// `Checksum::of_ir` front door folds. There is NO `Raw`/`RawDown`;
// every transform/predicate is the closed [`expr::Expr`] AST.
pub use model::ir::{
    validate_type_id_prefix, BackfillSetValue, CanonicalOpList, ColType, ColumnOrExpr,
    CommentTarget, CursorStability, EmptyContainerKind, ExclusionElement, ExclusionMethod,
    ExclusionOperator, GeneratedCol, IdentityCol, IndexElement, IndexMethod, IndexSortOrder,
    IndexStorageParams, IrClassification, IrColumn, IrConstraint, IrConstraintKind, IrDefault,
    IrFlagsOverride, IrIndex, IrJsonValue, IrMask, IrMaskKind, IrScalar, IrValue, IrVersionError,
    MigrationIr, Op, PartitionBoundValue, PartitionBounds, PartitionSpec, PerRowGenerator,
    RefAction, SafeI64, SafeU64, SequenceOwnedBy, SequenceRef, TableRuntimeOptions,
    TableRuntimeOptionsPatch, TableStrictness, ValueFormat, VectorMetric, CURRENT_IR_VERSION,
    EXPR_INVALID_NUMERIC, TYPE_ID_MAX_PREFIX_LEN,
};
// The fail-closed IR envelope load gate: deserialize →
// `ir_version` → `validate_ir` → server-stamped ownership → advisory checksum-hint
// compare. The loader's IR branch ([`render::lower::IrAuthor::load_and_lower`]) runs
// this gate and then lowers the validated, owned IR to migrations.
pub use model::load::{
    enforce_ir_ownership, hint_domain_uncomputable_field, load_ir_document,
    recompute_hint_domain_checksum, IrLoadError,
};
// The IR-path DDL Lower phase: compiles a validated, ownership-
// checked `MigrationIr` to migrations, reusing the SHARED snapshot-builder +
// declarative render seam so its SQL is byte-identical to the differ's path.
pub use render::lower::{
    FragmentGuardDenied, GuardedFragment, IrAuthor, IrGuardedLowerError, IrLowerError, LiveSchema,
    LoadAndLowerError, LoadAndLowerGuardedError, LoweredArtifact,
};
// The closed expression AST the IR's transform/predicate positions
// carry. Constructed in JS, serialized as data, NEVER parsed from text.
pub use model::expr::{
    BinaryOp, CaseBranch, CastTarget, Duration, Expr, ExtractField, PgExtractField, ScalarFn,
    SynthFn, UnaryOp,
};
// The STRUCTURAL expression-AST validator + the structured-error envelope.
// No parser, no fuzzer — a pure allow-list walk.
pub use model::validate::{
    validate_expr, validate_ir, validate_ir_resolved, validate_op, validate_op_resolved,
    AuthoringError, Dialect as ValidatorDialect, LogicalColumnContract, LogicalColumnContracts,
    LogicalColumnKey, TargetScope, UnsupportedKind, CODE_COLUMN_FACET_CONFLICT,
    CODE_DIALECT_SCOPE_PGONLY, CODE_DIALECT_UNSUPPORTED, CODE_EXPR_NOT_PORTABLE,
    CODE_OP_OUTSIDE_RECORDER, CODE_PARTITION_BOUNDS_ILL_FORMED, CODE_PARTITION_BOUNDS_NOT_TOTAL,
    CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED, CODE_PARTITION_HASH_DROP_UNDERIVABLE,
    CODE_PARTITION_KEY_COVERAGE, CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE, CODE_UNSUPPORTED,
    SPLIT_PART_MAX_N,
};
// The `op.*` DSL plan model. Distinct from the dry-run `MigrationPlan`
// (re-exported from `engine`): these are the ordered
// EXECUTION artifact + its steps.
pub use model::backfill::{
    BackfillSpec, CursorColumnContract, CursorComparison, CursorContract, CursorScalarType,
    CursorTuple, CursorTupleError,
};
pub use model::probe::{ExpectColumn, GuardDir, GuardProbe};
pub use render::plan::{
    AppliedPlan, DatabaseFeature, DatabaseRequirements, NotSingleStep, RollbackAssessment,
    SqliteRebuildSpec, SqliteSequencePolicy,
};
pub use render::step::{
    tables_touched_by, AlterPrimaryKeyStep, BindValue, DialectScope, PlanStep, RenameStep,
    StepReversibility, SynchronizeIdentityStep,
};
// The OFFLINE `--sql` plan preview. A pure,
// DB-free surfacing/formatting layer over the SQL `IrAuthor::lower_*` already
// lowers; DB-state-dependent ops are labeled `-- [runtime-resolved]`, never
// fabricated.
#[cfg(pg_seam)]
pub use apply::precondition::evaluate as evaluate_precondition;
pub use apply::precondition::PreconditionError;
pub use model::precondition::{CmpOp, OnUnmet, Precondition, PreconditionCheck};
pub use render::sql_preview::{
    render_ir_envelope_sql, render_ir_envelope_sql_statements, render_plan_sql, render_set_sql,
    PreviewOpts, RUNTIME_RESOLVED,
};
// `migrator_role_name` / `RoleError` are pure (identifier derivation + a shared
// error enum); the provisioning fns run over a PG `admin: &Client` — PG-only.
pub use apply::role::{migrator_role_name, RoleError};
