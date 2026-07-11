//! `zeroship-migrate` — zeroship's versioned DB migration engine for **creator
//! project databases** (design `docs/proposals/2026-06-16-db-migration-engine-design.md`).
//!
//! The crate also carries the JS-first authoring front-end under [`frontend`]:
//! V8-backed schema evaluation, op.* recording, type generation, and the
//! `zeroship-migrate-js` authoring CLI. The deploy/apply fast path still runs
//! through the native compio-postgres backend.
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

// Dead-code / unused-import allowance for the two PG-partial build shapes:
//
//  * `--no-default-features` (neither `native-pg` nor `host-pg`): the WHOLE PG
//    apply path is gated out, so its dialect-neutral helpers read as dead.
//  * `--features host-pg` (no `native-pg`): the GENERIC apply path compiles, but
//    the compio-CONCRETE legs stay `native-pg`-only — the CLI `runner`, the
//    `ir_apply`-postgres entries, standalone `rollback`/`run_expand`, role
//    provisioning helpers, and the shadow harness. The helpers those legs are the
//    sole callers of are unreachable in the addon build (they are Phase-C /
//    out-of-v1 surface), so they read as dead there too.
//
// Both are `not(feature = "native-pg")`. The DEFAULT (`native-pg`-on) build has
// every path reachable, so it stays strict and warning-clean — the allowance
// never relaxes the platform build.
#![cfg_attr(not(feature = "native-pg"), allow(dead_code, unused_imports))]

pub mod analysis;
pub mod apply;
pub mod approval;
pub mod command;
pub mod conn;
pub mod db_url;
pub mod engine;
#[doc(hidden)]
pub mod fault;
#[cfg(feature = "v8-host")]
pub mod frontend;
pub mod guard;
pub mod id;
pub mod model;
pub mod ops;
pub mod plan;
pub mod render;
// The V8-host seam (extraction Phase A). UNGATED — the traits name no
// `zeroship-runtime` / `v8` type, so they compile with zero runtime dep. Only the
// *consumers* (`frontend/`, `apply/backend/mysql/`) are `v8-host`-gated.
pub mod runtime_host;
// PG integration-test helpers (all consumers are `*_pg.rs` suites that connect to
// a live PG at :5440). PG-only, so gated with `native-pg`.
#[cfg(feature = "native-pg")]
#[doc(hidden)]
pub mod test_support;

/// Default/server builds do not link the raw standalone apply convenience.
///
/// ```compile_fail
/// let _raw = zeroship_migrate::apply_standalone;
/// ```
///
/// The standalone Trusted runner profile is likewise absent from default/server
/// builds.
///
/// ```compile_fail
/// let _trusted = zeroship_migrate::command::runner::RunProfile::Trusted;
/// ```
///
/// ```compile_fail
/// let _trusted_guard = zeroship_migrate::guard::GuardConfig::trusted;
/// ```
#[cfg(all(doctest, not(feature = "standalone-cli")))]
pub struct StandaloneCliFeatureAbsent;

// The V8-host seam types (extraction Phase A) — UNGATED. Runtime adapters impl
// these; the engine names only them (never a `zeroship_runtime` type).
pub use runtime_host::{
    AuthoringHost, GlobalsProfile, HostEvalError, HostPort, JsDriverHost, JsDriverTimeout,
    ModuleEntry, NetPolicy, RecorderPlatform, ReviewedAllowlist,
};

pub use analysis::{analyze, classify};

// ---------------------------------------------------------------------------
// Public API surface — re-exports (later plans depend on these names).
// ---------------------------------------------------------------------------

pub use analysis::analyze::{analyze, analyze_migration, Advisory, Severity};
pub use approval::{Approval, ApprovalScope};
#[cfg(all(test, feature = "v8-host"))]
pub use apply::backend::{MysqlFragmentDecision, MysqlFragmentEvent, MysqlFragmentHookAction};
// V8-free, driver-neutral re-exports (consumed by plugin-db / migrated per the
// decoupling design §7). Name no compio type and back the SQLite path too — ungated.
pub use apply::backend::{
    BackfillError, BackfillOutcome, CrossDeployObligations, DryRunError, DryRunReport,
    MigrationBackend, MigrationResult, OnlineSchemaChange, SeedError, ShadowConfig, ShadowDryRun,
};
// PG-seam re-exports (consumer-based gate: `PgSessionSnapshot` is a pure-`String`
// struct, but its only consumers are the PG session leaves). Behind the PG seam
// (`native-pg` OR `host-pg`) — the generic `PostgresBackend<D>` compiles on both.
#[cfg(pg_seam)]
pub use apply::backend::{PgSessionSnapshot, PostgresBackend};
// The driver-neutral `PgSession` seam types (§A). Public so a host (napi) driver
// can construct return values / binds, and so error consumers read the neutral
// `SeamError` (SQLSTATE in `.code`) instead of downcasting to the concrete
// `compio_postgres::Error`. On the whole PG seam — the addon (`host-pg`) is the
// primary consumer of these neutral types.
#[cfg(pg_seam)]
pub use apply::backend::postgres::seam::{FromSeam, SeamBind, SeamError, SeamRow, SeamValue};
#[cfg(pg_seam)]
pub use apply::backend::postgres::session::PgSession;
// MySQL backend re-exports (the V8-coupled live-MySQL surface — gated behind `zsv8`).
#[cfg(feature = "v8-host")]
pub use apply::backend::{
    deprovision_mysql_migrator_account, mysql_migration_lock_name,
    provision_mysql_migrator_account, provision_mysql_migrator_account_with_password, JsDriverConn,
    JsDriverError, MysqlBackend, MysqlGuardedFragment, MysqlMigratorAccount,
    MysqlMigratorAccountError, MysqlSessionSnapshot, RowSet,
};
pub use apply::backend::sqlite::{RebuildError, SqliteActorError, SqliteBackend};
#[cfg(feature = "native-pg")]
pub use apply::backend::postgres::online::PgOnline;
#[cfg(feature = "native-pg")]
pub use apply::backend::postgres::shadow::PgShadow;
pub use apply::baseline::{BaselineError, BaselineOutcome};
#[cfg(feature = "native-pg")]
pub use apply::backend::postgres::backfill::{
    backfill_progress, ensure_backfill_progress, list_backfills, run_backfill,
    run_backfill_bounded, BackfillProgress,
};
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
#[cfg(feature = "native-pg")]
pub use conn::connect;
pub use apply::drift::{
    diff_snapshots, AlteredObject, ChecksumDrift,
    ChecksumDriftReport, DriftError, DriftReport, OrphanJournal, StructuralDrift,
};
// `check_checksum_drift` reads the journal over a `&D: PgSession`; `snapshot_schema`
// introspects `pg_catalog` — both generic over the seam (SQLite has its own peers).
// On the whole PG seam.
#[cfg(pg_seam)]
pub use apply::drift::{check_checksum_drift, snapshot_schema};
pub use apply::executor::{
    ApplyError, ApplyOutcome, BackendError, LockMode, PreconditionVerdict,
    RollbackError, RollbackOptions, RollbackOutcome, RollbackRequest, RollbackTarget,
};
// `apply` is generic over the `PgSession` seam (§C.5 holdout 1) — on the whole PG
// seam. `rollback` is still `&Client`-typed (out of v1 scope) — native-pg only.
#[cfg(pg_seam)]
pub use apply::executor::apply;
#[cfg(feature = "native-pg")]
pub use apply::executor::rollback;
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
// The deploy-target dialect (§2.4.1) — re-exported so the control-plane deploy
// path can thread it into `IrAuthor::new` without depending on `zeroship-schema`.
pub use zeroship_schema::query::SqlDialect;
// Dialect-neutral journal types (the SQLite path constructs/imports these too).
pub use apply::journal::{
    AppliedEntry, HistoryEvent, HistoryKind, JournalError, JournaledKind, PendingContract,
    PendingContractRecord, PendingState, Phase, Resolution, RolledBackEntry,
};
// The PG journal free functions (each takes a `&D: PgSession` connection). The
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
// `history` / `status` are generic over the `PgSession` seam (§C.5 holdout 3) —
// on the whole PG seam so a host driver can drive the pending-migrations flow.
#[cfg(pg_seam)]
pub use ops::status::{history, status};
// The confined submit path is PG-only (§4.5); gated with `mod ops::submit`.
#[cfg(feature = "native-pg")]
pub use ops::submit::{submit_migration, Submission, SubmissionOutcome, SubmitError};
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
#[cfg(feature = "native-pg")]
pub use command::ir_apply::{
    apply_bundle_ir_postgres, apply_one_ir_file_postgres, apply_sealed, postgres_ir_apply_state,
};
#[cfg(feature = "standalone-cli")]
pub use command::ir_apply::apply_standalone;
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
#[cfg(feature = "native-pg")]
pub use apply::role::{deprovision_migrator, provision_migrator};
#[cfg(feature = "native-pg")]
pub use apply::backend::postgres::shadow::{
    dry_run, dry_run_declarative, dry_run_incremental, sweep_leaked_shadows,
};
#[cfg(feature = "native-pg")]
#[doc(hidden)]
pub use apply::backend::postgres::shadow::arm_panic_after_provision;
