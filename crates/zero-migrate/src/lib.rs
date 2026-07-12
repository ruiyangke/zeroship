//! `zero-migrate` — a versioned DB migration engine for **creator
//! project databases** (design `docs/proposals/2026-06-16-db-migration-engine-design.md`).
//!
//! The engine is runtime-free and V8-free. Authoring (the JS DSL to op-IR
//! envelope) and live Postgres/MySQL execution run in the Node host and reach
//! the engine through the `zero-migrate-node` napi bridge; the Postgres apply
//! path is the driver-neutral [`SqlSession`] seam. In-process SQLite is the one
//! backend the engine drives directly.
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

// The SQL security layer (`analysis` + `guard`) moved to the `zero-migrate-guard`
// crate (redesign step 3b). Re-export its modules under their historical
// `crate::{analysis,guard}` paths so the engine's ~dozens of
// `crate::guard::…` / `crate::analysis::…` references keep resolving unchanged.
pub use zero_migrate_guard::{analysis, guard};
pub mod apply;
pub mod approval;
pub mod command;
pub mod conn;
pub mod db_url;
pub mod engine;
#[doc(hidden)]
pub mod fault;
// The typed-id (base62/UUIDv7) machinery moved to the `zero-migrate-ir` leaf crate
// (redesign step 3a); re-export it under its historical `crate::id` path.
pub use zero_migrate_ir::id;
// The deploy-bundle migration-file record + content-addressed hash, vendored
// byte-identically from the upstream bundle layer (extraction Phase B) so the build
// front-end emits bundle entries without a the upstream bundle layer normal-graph dep.
pub mod manifest_entry;
pub mod model;
// The reviewed-allowlist net-policy security types, vendored byte-identically
// from the upstream core (extraction Phase B) so the recorder-sandbox / MySQL
// JS-driver `NetPolicy` names no the upstream core type.
pub mod net_policy;
pub mod ops;
pub mod plan;
pub mod render;
// The dialect-neutral network driver seam (`SqlSession`) — the ONE injected
// runtime dependency the network-dialect backends (`PostgresBackend`, and the
// forthcoming `MysqlBackend`) are generic over. SQLite does NOT ride it (it is an
// in-process rusqlite actor). Gated on `pg_seam` (its only current implementor is
// the compio PG adapter, lit by the `host-pg` feature); a `--no-default-features`
// build keeps a lean core.
#[cfg(pg_seam)]
pub mod driver;
// The schema-authority core (DDL builders, diff classifier, sentinel codec,
// schema-shape descriptors) — dissolved in from the former `zero-migrate-schema`
// leaf crate (redesign step 3c). The data-plane query language that used to ride
// along there had zero engine callers and was deleted; only the write/diff/describe
// layer the engine uses survives here.
pub mod schema;

// The guard behaviour-lock suite (moved in from `zero-migrate-guard`'s
// `guard/mod.rs` at redesign step 3b) — an in-crate test module so it can drive
// the guard through the engine's `render::lower` / `conn` internals.
#[cfg(test)]
mod guard_vendor_lower_tests;

pub use analysis::{analyze, classify};

// ---------------------------------------------------------------------------
// Public API surface — re-exports (later plans depend on these names).
// ---------------------------------------------------------------------------

pub use analysis::analyze::{analyze, analyze_migration, Advisory, Severity};
pub use approval::{Approval, ApprovalScope};
// V8-free, driver-neutral re-exports (consumed by plugin-db / migrated per the
// decoupling design §7). Name no compio type and back the SQLite path too — ungated.
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
#[cfg(pg_seam)]
pub use driver::{Bind, ColIndex, DbError, FromValue, Row, SqlSession, Value};
pub use apply::backend::sqlite::{RebuildError, SqliteActorError, SqliteBackend};
pub use apply::baseline::{BaselineError, BaselineOutcome};
pub use plan::author::{
    AuthorError, AuthorRequest, Column, DeterministicAuthor, MigrationAuthor, RawSqlAuthor,
};
pub use analysis::classify::{
    classify, drop_index_targets, relations_touched, DdlKind, DropIndexTarget, OwnershipNeed,
    ParseError, StatementClass, TouchedRelation,
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
pub use conn::{ConnectError, ExecutorConfig, PgConfinement};
pub use apply::drift::{
    diff_snapshots, AlteredObject, ChecksumDrift,
    ChecksumDriftReport, DriftError, DriftReport, OrphanJournal, StructuralDrift,
};
// `check_checksum_drift` reads the journal over a `&D: SqlSession`; `snapshot_schema`
// introspects `pg_catalog` — both generic over the seam (SQLite has its own peers).
// On the whole PG seam.
#[cfg(pg_seam)]
pub use apply::drift::{check_checksum_drift, snapshot_schema};
pub use apply::executor::{
    ApplyError, ApplyOutcome, BackendError, LockMode, PreconditionVerdict,
    RollbackError, RollbackOptions, RollbackOutcome, RollbackRequest, RollbackTarget,
};
// `apply` is generic over the `SqlSession` seam (§C.5 holdout 1) — on the whole PG
// seam. `rollback` is still `&Client`-typed (out of v1 scope) — native-pg only.
#[cfg(pg_seam)]
pub use apply::executor::apply;
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
pub use model::profile::{
    AuthorPrimaryKeyPolicy, DataSecurityConfig, DestructiveOps, IndexCreation,
    InjectedSystemColumnPolicy, InjectedSystemIndexPolicy, OperationalConfig, PolicyCapabilities,
    PolicyKnobSemantics, PolicyMeet, PolicyPolarity, PolicyProfile, PrimaryKeyAuthorPolicy,
    RoleAttribute, RoleCapabilityConfig, SealError, SealVerifier, SealedPosture, SealedProfile,
    TablePrimaryKeyPolicy, TableRewrite, TableSystemShapePolicy, seal_effective_profile,
    CONFINED_PROFILE_TOML, PLATFORM_PROFILE_TOML,
};
pub use model::table_shape::{resolve_create_table_policy, TableShapeError};
// The deploy-target dialect (§2.4.1) — re-exported so an embedding host's deploy
// path can thread it into `IrAuthor::new` without depending on `zero-migrate-schema`.
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
    applied, applied_count, ensure_journal, history as journal_history,
    latest_completed_checksums, net_rolled_back, outstanding_pending_contracts,
    record_baseline, record_completed, record_rolled_back, record_started,
    resolve_pending_contract, superseded_versions,
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
    BlockedPlan, MigrationStatus, PendingContractStatus, StatusError,
};
// `history` / `status` are generic over the `SqlSession` seam (§C.5 holdout 3) —
// on the whole PG seam so a host driver can drive the pending-migrations flow.
#[cfg(pg_seam)]
pub use ops::status::{history, status};
// The confined submit path is PG-only (§4.5); gated with `mod ops::submit`.
pub use plan::manifest::{
    compute_manifest, verify_manifest, ManifestError, ManifestHash, MismatchKind,
};
pub use model::migration::{
    migration_id_for_version, Checksum, ChecksumInput, IdError, Migration, MigrationFlags,
    MigrationId, OnlinePhase, MIGRATION_PREFIX,
};
pub use model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, ExtensionSnapshot, GeneratedColumnSnapshot,
    IndexElementSnapshot, IndexSnapshot, NamedTypeSnapshot, PartitionSnapshot, RoleSnapshot,
    SchemaObjectSnapshot, SchemaSnapshot, SequenceDataTypeSnapshot, SequenceSnapshot,
    TableSnapshot, ViewSnapshot,
};
// The `op.*` portable IR (§2.1/§2.3/§2.5): the migration document, the closed
// `Op` enum, the constrained numeric scalar, and the canonical op-list the
// `Checksum::of_ir` front door folds. There is NO `Raw`/`RawDown` (property A);
// every transform/predicate is the closed [`expr::Expr`] AST.
pub use model::ir::{
    CanonicalOpList, ColType, ColumnOrExpr, CommentTarget, EmptyContainerKind, ExclusionElement,
    ExclusionMethod, ExclusionOperator, GeneratedCol, IdentityCol, IndexElement, IndexMethod,
    IndexSortOrder, IndexStorageParams, IrClassification, IrColumn, IrConstraint,
    IrConstraintKind, IrDefault, IrFlagsOverride, IrIndex, IrJsonValue, IrMask, IrMaskKind,
    IrScalar, IrValue, IrVersionError, MigrationIr, Op, PartitionBoundValue, PartitionBounds,
    PartitionSpec, RefAction, SafeI64, SafeU64, SequenceOwnedBy, SequenceRef, TableRuntimeOptions,
    TableRuntimeOptionsPatch, TableStrictness, VectorMetric,
    CURRENT_IR_VERSION, EXPR_INVALID_NUMERIC,
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
    apply_bundle_ir_sqlite, apply_bundle_ir_sqlite_catalog, discover_ir_files,
    IrDiscoveryError, PostgresIrApplyError, PostgresIrApplyOutcome, PostgresIrApplyState,
    SealedApplyError, SqliteIrApplyError, SqliteIrApplyOutcome,
};
// The PG `.ir.json` apply entry points take `&PostgresBackend<'_>` — PG-only.
// The IR-path DDL Lower phase (§6/§6.4/§6.5): compiles a validated, ownership-
// checked `MigrationIr` to migrations, reusing the SHARED snapshot-builder +
// declarative render seam so its SQL is byte-identical to the differ's path.
pub use render::lower::{
    FragmentGuardDenied, GuardedFragment, IrAuthor, IrGuardedLowerError, IrLowerError, LiveSchema,
    LoadAndLowerError, LoadAndLowerGuardedError, LoweredArtifact,
};
// The closed expression AST (§3.3.1) the IR's transform/predicate positions
// carry. Constructed in JS, serialized as data, NEVER parsed from text.
pub use model::expr::{
    BinaryOp, CaseBranch, CastTarget, Duration, Expr, ExtractField, PgExtractField, ScalarFn,
    SynthFn, UnaryOp,
};
// The STRUCTURAL expression-AST validator + the structured-error envelope
// (§3.3.1.1 / §8.8). No parser, no fuzzer — a pure allow-list walk.
pub use model::validate::{
    validate_expr, validate_ir, validate_ir_resolved, validate_op, validate_op_resolved,
    AuthoringError, Dialect as ValidatorDialect, TargetScope, UnsupportedKind,
    CODE_COLUMN_FACET_CONFLICT, CODE_DIALECT_SCOPE_PGONLY, CODE_DIALECT_UNSUPPORTED,
    CODE_EXPR_NOT_PORTABLE, CODE_OP_OUTSIDE_RECORDER, CODE_PARTITION_BOUNDS_ILL_FORMED,
    CODE_PARTITION_BOUNDS_NOT_TOTAL, CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED,
    CODE_PARTITION_HASH_DROP_UNDERIVABLE, CODE_PARTITION_KEY_COVERAGE,
    CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE, CODE_UNSUPPORTED, SPLIT_PART_MAX_N,
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
    render_ir_json_sql, render_ir_json_sql_statements, render_plan_sql, render_set_sql,
    PreviewOpts, RUNTIME_RESOLVED,
};
pub use apply::precondition::PreconditionError;
#[cfg(pg_seam)]
pub use apply::precondition::evaluate as evaluate_precondition;
pub use model::precondition::{CmpOp, OnUnmet, Precondition, PreconditionCheck};
// `migrator_role_name` / `RoleError` are pure (identifier derivation + a shared
// error enum); the provisioning fns run over a PG `admin: &Client` — PG-only.
pub use apply::role::{migrator_role_name, RoleError};
