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
//! ([`MigrationEngine::apply`](crate::engine::MigrationEngine::apply)) — transactional + two-phase
//! non-transactional with idempotent recovery, the guard wired in front of
//! every `up`, and a drift/tamper checksum check — the least-privilege
//! `migrator` role (this vendor's, so its derivation lives in the PostgreSQL
//! backend crate rather than here), and the **public
//! authoring pipeline + engine API** ([`plan::author`] +
//! [`engine`]).
//!
//! # The pipeline
//!
//! ```text
//! author -> plan (lint) -> gate (approval) -> executor apply (guard + role)
//! ```
//!
//! 1. an **author** ([`MigrationAuthor`]) produces versioned [`Migration`]s.
//!    [`DeterministicAuthor`] handles the trivial additive set (create table,
//!    add column, create index) with no AI; [`RawSqlAuthor`] is the **AI-author
//!    hook** — the AI/builder generates complex migrations (renames, type
//!    changes, backfills, expand-contract) *externally* and this engine
//!    validates + executes them. The engine never calls an LLM.
//! 2. [`MigrationEngine::plan`] runs the registered [`guard::MigrationGuard`] read-only and
//!    returns a [`MigrationPlan`] (the dry-run/preview): destructive flags,
//!    approval requirement, and guard *denials*.
//! 3. [`MigrationEngine::apply`] is the **gate** ([`Approval`]): it refuses a
//!    denied plan, refuses a destructive plan without approval, and otherwise
//!    delegates to the executor's apply shell — which **independently re-runs the
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
//! - **Line 1 — the registered backend's guard ([`guard::MigrationGuard`]).** Every statement is parsed
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

// The NEUTRAL guard seam — `GuardConfig`, `GuardMode`, `GuardError`, `GuardOutcome`,
// `MigrationGuard` and the structured-IR data-security walk. Re-exported under the
// historical `crate::guard::…` path so the engine's dozens of references keep
// resolving unchanged.
//
// This used to read `pub use zero_migrate_guard::{analysis, guard};`, and that one
// line was the engine's whole coupling to a PostgreSQL parser: `zero-migrate-guard`
// was the `libpg_query` deny-list + classifier + analyzers, and re-exporting it here
// put `SqlGuard`, `DdlKind` and `analyze` on the neutral engine's PUBLIC API. The
// crate is gone — all of it needed `libpg_query`, so all of it was PostgreSQL's, and
// it now lives in `zero-migrate-postgres`. Core reaches a guard the same way it
// reaches a renderer: `render::backends::guard_for`, through the registry, by open
// dialect id.
pub use zero_migrate_backend::guard;
pub mod apply;
// The caller's approval decision now lives with the backend contract, whose
// `OnlineSchemaChange::run_online` names it. Re-exported here so every
// `crate::approval::{Approval, ApprovalScope}` reference resolves unchanged.
pub use zero_migrate_backend::approval;
// The per-run executor configuration. `ExecutorConfig` is the single most-named
// type in the backend contract — every one of `MigrationBackend`'s I/O methods
// takes a `&ExecutorConfig` — so it moved down to sit with the trait it is an
// argument of. Re-exported so every `crate::conn::…` and `zero_migrate::conn::…`
// reference resolves unchanged.
pub use zero_migrate_backend::conn;
pub mod db_url;
pub mod engine;
// The crash-simulation seam moved down to the backend contract: all three
// `MigrationBackend` implementations trip it on their own apply paths, so it has to
// sit below the vendors rather than above them. Re-exported so every
// `zero_migrate::fault::…` path resolves unchanged.
#[doc(hidden)]
pub use zero_migrate_backend::fault;
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
// in-process rusqlite actor). Its only current implementor is the host PG adapter.
//
// It LIVES in `zero-migrate-backend` — it is a contract, not engine logic, and its
// only dependency was `std`. Re-exported under its historical `crate::driver` path
// (the same shim idiom `model/mod.rs` uses for `zero_migrate_ir::ir`) so every
// `crate::driver::…` and `zero_migrate::driver::…` reference resolves unchanged.
pub use zero_migrate_backend::driver;
// The schema-authority core (DDL builders, diff classifier, sentinel codec,
// schema-shape descriptors). The data-plane query language that used to ride
// alongside had zero engine callers and was deleted; only the write/diff/describe
// layer the engine uses survives here.
pub mod schema;

#[cfg(test)]
pub(crate) mod test_fixtures;

// ---------------------------------------------------------------------------
// Public API surface — re-exports.
// ---------------------------------------------------------------------------

// `Advisory` and `Severity` are the NEUTRAL advisory vocabulary and come from the
// backend contract, which is where they are defined. The `analyze` /
// `analyze_migration` free functions that used to sit beside them are GONE from this
// root: they ran the `libpg_query` analyzers on whatever they were handed, so a MySQL
// or SQLite statement came back as a parse failure reported as a clean, empty
// advisory list. `advisories_for_sql` below is the replacement — it asks the
// REGISTERED backend, and a backend with no analyzer answers `NotAnalyzed` rather
// than "nothing found". PostgreSQL's analyzers are still exactly where they were, at
// `zero_migrate_postgres::analysis::analyze`.
pub use approval::{Approval, ApprovalScope};
pub use zero_migrate_backend::advisory::{Advisory, Severity};
// The ADVISORY seam: ask the registered backend for an analysis, never a vendor by
// name. `advisories_for_sql` and `analyzer_absence` are the two entry points a host
// needs; the verdict types come with them because a caller cannot handle
// `NotAnalyzed` without being able to name it.
//
// This is what the old root-level `analyze` was retired in favour of, and the
// retirement is now complete: `zero-migrate-guard` has moved into
// `zero-migrate-postgres`, so the parser-bearing entry point is gone from this root
// exactly as this comment said it would be.
pub use render::backends::{advisories_for_sql, analyzer_absence};
pub use zero_migrate_backend::advisory::{
    AdvisoryVerdict, AnalyzerAbsent, IndexCoverage, OperationalAdvisor,
};
// V8-free, driver-neutral re-exports. Name no host-driver type and back the
// SQLite path too — ungated.
pub use apply::backend::{
    BackendCapability, BackfillError, BackfillOutcome, CrossDeployObligations, DryRunError,
    DryRunReport, MigrationBackend, MigrationResult, OnlineSchemaChange, ShadowConfig,
    ShadowDryRun,
};
// `PostgresBackend` IS NOT RE-EXPORTED HERE, and neither is any other vendor's.
// It lives in `zero-migrate-postgres` with the rest of the PostgreSQL execution
// half, and a `pub use zero_migrate_postgres::PostgresBackend` at this root would be
// core naming a vendor CRATE outside the registry — the thing
// `tests/dialect_matrix/core_names_no_vendor_crate.rs` exists to forbid. Closing one
// coupling by opening the other would have been a wash. A host that wants a
// PostgreSQL backend names `zero_migrate_postgres::PostgresBackend`, exactly as it
// already names `zero_migrate_sqlite::SqliteBackend` and
// `zero_migrate_mysql::MysqlBackend`.
// The driver-neutral `SqlSession` seam types (the engine-root `crate::driver`
// module). Public so a host (napi) driver can construct return values / binds,
// and so error consumers read the neutral `DbError` (SQLSTATE in `.sqlstate`). The
// napi addon is the primary consumer of these neutral types. MySQL rides the same
// seam; SQLite does NOT (in-process rusqlite).
// `ParseError` is the NEUTRAL parse-failure vocabulary and stays. The statement
// CLASSIFIER that used to be re-exported beside it does not: `classify`, `DdlKind`,
// `StatementClass`, `TouchedRelation`, `OwnershipNeed`, `DropIndexTarget`,
// `relations_touched` and `drop_index_targets` are all `libpg_query` vocabulary that
// only PostgreSQL can populate, so putting them on the neutral engine's public API
// invited exactly one reading — that a `DdlKind` describes any backend's statement.
// They live at `zero_migrate_postgres::analysis::classify`, which says whose they
// are.
//
// ── `SqliteBackend`, `SqliteActorError` and `RebuildError` are NOT re-exported here
// any more, and they did not move to another path in core — they left the crate.
// The SQLite execution half is `zero_migrate_sqlite::backend` now, and a
// `pub use zero_migrate_sqlite::SqliteBackend` here would be core naming a vendor
// CRATE outside the registry, which is exactly what
// `tests/dialect_matrix/core_names_no_vendor_crate.rs` forbids: closing one coupling
// by opening the other would have been a wash. MySQL went the same way one commit
// earlier. A host that wants the SQLite backend names the vendor crate, as
// `zero-migrate-node`'s bridge does.
pub use apply::baseline::{BaselineError, BaselineOutcome};
pub use apply::drift::{
    diff_snapshots, diff_snapshots_with_index_aliases, AlteredObject, ChecksumDrift,
    ChecksumDriftReport, DriftError, DriftReport, OrphanJournal, StructuralDrift,
};
pub use conn::{ConfinementConfig, ConnectError, ExecutorConfig, PostgresConfinement};
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
    desired_snapshot_for_dialect, AcceptedIndexAlias, CollectionDescriptor, DeclarativeAuthor,
    DeclarativeError, DeclarativePlan, DesiredSchema, FieldDescriptor, IndexDescriptor, RenameHint,
    TableRebuild,
};
pub use render::expand_contract::{
    ExpandContractAuthor, ExpandContractError, ExpandContractPlan, OnlineIntent,
};
pub use zero_migrate_backend::guard::ParseError;
// `check_checksum_drift`, `snapshot_schema` and `resolve_view_bodies` are NOT
// re-exported here. They read `pg_catalog`/`information_schema` and drive a
// PostgreSQL savepoint probe; promising them at the crate root said the engine
// offers them, when what the engine offers is whatever the REGISTERED backend
// implements. They live at `zero_migrate_postgres::backend::drift_sql`, reached by that
// name. The neutral surface is
// `MigrationBackend::{check_checksum_drift, snapshot_schema}`.
pub use apply::executor::{
    ApplyError, ApplyOutcome, BackendError, LockMode, PreconditionVerdict, RollbackError,
    RollbackOptions, RollbackOutcome, RollbackRequest, RollbackTarget,
};
// The rollback verb, and the planner that decides every refusal before it runs.
// Generic over `MigrationBackend`, so it runs on every dialect through one body.
pub use apply::executor::{
    plan_rollback, plan_rollback_with_inverse_plans, rollback, rollback_with_lock,
    rollback_with_lock_and_inverse_plans, AppliedRecord, RollbackPlan,
};
// `apply` is generic over `MigrationBackend` too, exactly like `rollback` above.
// It used to take the `SqlSession` seam and build a `PostgresBackend` inside the
// executor, which made this "neutral" entry PostgreSQL-only by construction; the
// caller now supplies the backend. Its two vendor-constructing siblings
// (`apply_with_lock`, `apply_with_lock_mysql`) were deleted rather than converted
// — nothing called them.
pub use apply::executor::apply;
// The OFFLINE ops→snapshot fold. Pure, no
// DB: replay an ordered `Op` list into the EXISTING `SchemaSnapshot` (drift.rs),
// the offline companion of `snapshot_schema`. The type-generation path emits the
// `env.db` types + runtime descriptor from this. See `fold.rs`.
// The NEUTRAL guard vocabulary only. `SqlGuard`, `GuardReport` and `flags_for` came
// off this list when `zero-migrate-guard` dissolved: all three are `libpg_query`
// machinery, and re-exporting them here made the neutral engine's public API hand out
// PostgreSQL's parser to every downstream caller — the same shape, and the same
// mistake, as the `MysqlGuard`/`PgGuard`/`SqliteGuard` re-exports described below.
// They are reachable at `zero_migrate_postgres::guard`, which says whose they are.
// A caller that wants "this project's line-1" rather than "PostgreSQL's" asks the
// registry through `render::backends::guard_for`.
pub use guard::{GuardConfig, GuardError, GuardMode, GuardOutcome, MigrationGuard};
// ── The three per-vendor guard TYPES are NOT re-exported here any more.
//
// `MysqlGuard`, `PgGuard` and `SqliteGuard` were `pub use`d at this crate root, and
// that made the engine's PUBLIC API name three vendor crates by name — the widest
// possible form of the coupling, because a re-export is reachable by every downstream
// caller and nothing tells that caller it is holding a vendor's type.
//
// Nothing moved and nothing was reimplemented: each guard still lives in its own
// backend crate and is still built by that crate's `BackendVendor::guard` factory.
// What went away is the SECOND door. [`guard_for`] below is the first, it selects
// through the vendor registry, and it was already the door the napi addon and the
// engine's own apply path used.
//
// The re-exports had no non-test caller in `crates/`, `sdks/` or `packages/`
// (measured, not assumed): `MysqlGuard` had zero callers anywhere, and the eight
// `PgGuard`/`SqliteGuard` sites were all in this crate's own integration tests,
// which now build the same guard the same way the engine does. So no behaviour left
// with the lines — the tests assert on the same guards, selected through the
// registry instead of constructed by name.
//
// The property is pinned by `tests/dialect_matrix/core_names_no_vendor_crate.rs`,
// which is a ratchet rather than prose: `lib.rs` is on its DENY side, so putting a
// vendor crate back at the crate root is a red test, not a review question.

/// Select the LINE-1 guard for a config's dialect, from that dialect's own vendor
/// crate.
///
/// Kept as a crate-root function because it is public API: the napi addon calls
/// `zero_migrate::guard_for` across the crate boundary, and the vendor registry it now
/// delegates to is `pub(crate)`. What changed is not the signature but the OWNER of
/// the dispatch — it is no longer a second closed-identity match inside the guard crate,
/// able to disagree with the renderer registry, and it is no longer able to hand a
/// dialect a guard that dialect did not write.
///
/// # Errors
/// None; registration guarantees a guard for each shipping backend.
#[must_use]
pub fn guard_for(cfg: &GuardConfig) -> Box<dyn MigrationGuard> {
    render::backends::guard_for(cfg)
}
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
// `fold_to_field_defs` is NOT in this list any more. Step 4 consumer 3 of
// `docs/proposals/single-fold-and-effects.md` section G deleted it; the wire `FieldDef`
// map it produced is now `single_fold::fold(…)?.project_field_defs()`, reached through
// `render::fold::single_fold`. The replacement is deliberately not a renamed function:
// one traversal decides what an op means and a projection READS the value, which is the
// shape the proposal's decision 1 asks for and the shape a second walker would hide.
pub use render::fold::{
    descriptors_to_create_ops, fold_ops, fold_ops_onto, history_carries_dialectal_ops,
    recover_check_facet, FoldError, ProduceError, RecoveredCheck,
};
pub use zero_migrate_policy::EffectivePolicy;
// The `gen-types` schema-artifact emitter: fold a schema source (op.* migrations or
// a declared `CollectionDescriptor` set) into the two co-emitted projections
// (`schema.runtime.json` v1 descriptor + generated `env.db.ts`), plus the in-memory
// `--check` drift gate. Both sources route through the SAME renderer, so output is
// byte-identical for equivalent schemas.
pub use render::gen_types::{
    check_artifacts, diff_artifacts, render_artifacts, render_artifacts_from_descriptors,
    render_schema_export, render_schema_export_from_descriptors, CheckDiff, GenTypesError,
    GeneratedArtifacts, SchemaExport, DEFAULT_PROJECT_SCHEMA, ENV_DTS_FILE,
    RUNTIME_DESCRIPTOR_FILE,
};
// The OPEN dialect identity and the backend contract keyed by it. A backend is
// named by a `DialectId`, describes itself with a `BackendDescriptor`, and is
// admitted by a `BackendRegistry` that refuses a duplicate id rather than
// picking a winner. Re-exported so an embedding host names one vocabulary.
pub use zero_migrate_ir::backend::{
    BackendDescriptor, BackendRegistry, Capability, CapabilitySet, IdentifierLimit, Limits,
    RegistryError,
};
pub use zero_migrate_ir::dialect::{DialectId, DialectSet, MYSQL, POSTGRES, SQLITE};

/// The backends THIS BUILD ships, validated into a [`BackendRegistry`].
///
/// The vendors are separate crates now (`zero-migrate-postgres`,
/// `zero-migrate-sqlite`, `zero-migrate-mysql`) and this engine names each of them
/// exactly once, in `render::backends::VENDORS`. That list is what replaced the
/// hard-coded three-arm identity match; this function is how a host asks
/// what it got, and it answers by running the leaf crate's own
/// [`BackendRegistry::build`] over the shipping descriptors rather than by restating
/// the id rule here.
///
/// It is derived from the vendors actually compiled in, so the contract crate owns
/// no parallel shipping list that can drift from the build's composition.
///
/// # Panics
///
/// Never in a shipped build: the shipping ids are constants and the test above
/// proves they satisfy the rule. The `expect` is here so a fourth backend added with
/// a bad or colliding id fails loudly at first use rather than being dropped.
#[must_use]
pub fn shipping_backends() -> BackendRegistry {
    render::backends::VENDORS
        .descriptors()
        .expect("the shipping backend crates must declare well-formed, distinct dialect ids")
}
// Dialect-neutral journal types (the SQLite path constructs/imports these too).
pub use apply::journal::{
    AppliedEntry, HistoryEvent, HistoryKind, JournalError, JournaledKind, PendingContract,
    PendingContractRecord, PendingState, Phase, Resolution, RolledBackEntry,
};
// The PG journal free functions are NOT re-exported here. They stood at the crate
// root as if they were THE engine's journal, and most had no consumer outside this
// crate at all — those are simply gone from the public surface. MySQL and SQLite
// have their own peer `journal_sql.rs` modules, and no caller reaching
// `zero_migrate::applied` could ever have got one. The ones with real callers live
// at `zero_migrate_postgres::backend::journal_sql`, reached by that name; the neutral
// surface is `MigrationBackend`'s journal methods.
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
// `status_via_backend` / `history_via_backend` live at `ops::status`, reached
// through the module path rather than promised at the crate root: the root once
// exported a `status` and a `history` that took a raw connection plus a dialect
// argument and then read PostgreSQL's journal regardless of what that argument
// said.
// The confined submit path is PG-only; gated with `mod ops::submit`.
pub use model::migration::{
    migration_id_for_version, Checksum, ChecksumInput, IdError, Migration, MigrationFlags,
    MigrationId, OnlinePhase, MIGRATION_PREFIX,
};
// The NEUTRAL schema model of `docs/proposals/single-fold-and-effects.md` section D:
// derived `PartialEq` on the model, vendor facts in a side table they cannot be reached
// from, and one NAMED comparator per question instead of one hand-written `eq` every
// consumer silently inherits. No consumer reads it yet - that is step 4.
pub use model::schema_model;
pub use model::schema_model::{
    column_shape_identity, constraint_shape_identity, drift_identity, index_pairing_identity,
    index_shape_identity, rename_equivalence_identity, table_shape_identity, ColumnKey,
    IndexElementKey, IndexKey, SchemaModel, TableKey, VendorFacts,
};
pub use model::snapshot::{
    ColumnCollationSnapshot, ColumnSnapshot, ConstraintSnapshot, ExtensionSnapshot, FunctionKey,
    FunctionSnapshot, GeneratedColumnSnapshot, GeneratedKindSnapshot, IdDefaultSnapshot,
    IndexElementSnapshot, IndexSnapshot, MysqlPhysicalType, MysqlTextStorageSnapshot,
    NamedTypeSnapshot, PartitionSnapshot, PolicyKey, PolicySnapshot, RoleSnapshot,
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
    ColumnGeneration, FragmentGuardDenied, GuardedFragment, IrAuthor, IrGuardedLowerError,
    IrLowerError, LiveSchema, LoadAndLowerError, LoadAndLowerGuardedError, LoweredArtifact,
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
    AuthoringError, LogicalColumnContract, LogicalColumnContracts, LogicalColumnKey, TargetScope,
    UnsupportedKind, CODE_COLUMN_FACET_CONFLICT, CODE_DIALECT_SCOPE_PGONLY,
    CODE_DIALECT_UNSUPPORTED, CODE_EXPR_NOT_PORTABLE, CODE_OP_OUTSIDE_RECORDER,
    CODE_PARTITION_BOUNDS_ILL_FORMED, CODE_PARTITION_BOUNDS_NOT_TOTAL,
    CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED, CODE_PARTITION_HASH_DROP_UNDERIVABLE,
    CODE_PARTITION_KEY_COVERAGE, CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE, CODE_UNSUPPORTED,
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
    TableRebuildSpec,
};
pub use render::step::{
    tables_touched_by, AlterPrimaryKeyStep, BindValue, DialectScope, PlanStep, RenameStep,
    StepReversibility, SynchronizeIdentityStep,
};
// The precondition VOCABULARY is neutral and stays exported. Its PostgreSQL
// EVALUATOR is not exported: `evaluate` and `PreconditionError` used to be aliased
// here as `evaluate_precondition`, promising one vendor's implementation as neutral
// crate API. Both had zero consumers, and a caller wanting to evaluate a
// precondition should go through `MigrationBackend::evaluate_preconditions`, which
// is what routes to the registered backend.
pub use model::precondition::{CmpOp, OnUnmet, Precondition, PreconditionCheck};
// The OFFLINE `--sql` plan preview. A pure,
// DB-free surfacing/formatting layer over the SQL `IrAuthor::lower_*` already
// lowers; DB-state-dependent ops are labeled `-- [runtime-resolved]`, never
// fabricated.
pub use render::sql_preview::{
    render_ir_envelope_sql, render_ir_envelope_sql_onto, render_ir_envelope_sql_statements,
    render_plan_sql, render_set_sql, PreviewOpts, RUNTIME_RESOLVED,
};

/// Compiles the Rust examples in `docs/embedding.md` as doctests.
///
/// `#[cfg(doctest)]` means this item exists only while rustdoc is collecting
/// doctests, so the guide's prose never lands in the published API docs while its
/// code is still compiled against the real crate. That is the whole point: the
/// embedding guide is the Rust half of the public surface, and until this existed
/// nothing compiled it, so a rename could rot every example in it and leave CI
/// green.
///
/// COVERAGE IS PARTIAL, and worth knowing before trusting a green run: the guide
/// has seven Rust fences and **three** of them are compiled. The rest are
/// ```` ```rust,ignore ```` and are neither compiled nor run, so a rename can still
/// rot them while CI stays green — the exact failure this item was added to
/// prevent, narrowed rather than eliminated.
///
/// The remaining four are not one job. Three need a live backend, an engine and a
/// config to exist before they say anything (`recover_inflight_ddl`,
/// `resolve_pending_contract`, the `PostgresBackend`/`MysqlBackend` pair), so
/// compiling them means standing up fake infrastructure whose drift would then need
/// its own guard. The fourth is a `trait SqlSession` DEFINITION quoted for shape:
/// compiling it would declare a SECOND trait that can silently diverge from the
/// real one while still passing, which is worse than leaving it ignored.
///
/// Where a fragment only lacks a binding, rustdoc's `# ` prefix hides the setup and
/// the fence becomes real coverage — that is how the policy example was closed.
/// Note the cost, since `embedding.md` is also read as plain Markdown in the repo:
/// hidden lines are invisible in rustdoc but VISIBLE there, so each one is
/// boilerplate a human reader pays for. Prefer making genuinely informative setup
/// visible (the policy example shows its charter string) and hiding only `fn main`
/// scaffolding.
///
/// The TypeScript docs are gated the same way from the other side, by the
/// `doc-examples` tests in both JS packages.
#[cfg(doctest)]
#[doc = include_str!("../../../docs/embedding.md")]
pub struct EmbeddingGuideDocTests;
