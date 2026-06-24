//! `zeroship-migrate` — zeroship's versioned DB migration engine for **creator
//! project databases** (design `docs/proposals/2026-06-16-db-migration-engine-design.md`).
//!
//! This crate implements the **security core** + the **migration unit** (the
//! migration data types §2.1 and the parse-time SQL security guard §1.4
//! deny-list / §1.5 cross-schema confinement), and the **Postgres executor**
//! (§2.3): the append-only journal ([`journal`]), the project advisory lock,
//! and the apply flow ([`executor::apply`]) — transactional + two-phase
//! non-transactional with idempotent recovery, the guard wired in front of
//! every `up`, and a drift/tamper checksum check — the least-privilege
//! `migrator` role ([`role`]), and the **public authoring pipeline + engine
//! API** ([`author`] + [`engine`]).
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
//!    delegates to [`executor::apply`] — which **independently re-runs the
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

pub mod analyze;
pub mod approval;
pub mod author;
pub mod backend;
pub mod backend_sqlite;
pub mod backfill;
pub mod baseline;
pub mod classify;
pub mod db;
pub mod declarative;
pub mod dml;
pub mod drift;
pub mod engine;
pub mod expand_contract;
pub mod executor;
pub mod expr;
#[doc(hidden)]
pub mod fault;
pub mod guard;
pub mod ir;
pub mod ir_author;
pub mod ir_load;
pub mod journal;
pub mod loader;
pub mod manifest;
pub mod migration;
pub mod plan;
pub mod precondition;
pub mod role;
pub mod shadow;
pub mod squash;
pub mod status;
pub mod submit;
pub mod validate;

// ---------------------------------------------------------------------------
// Public API surface — re-exports (later plans depend on these names).
// ---------------------------------------------------------------------------

pub use analyze::{analyze, analyze_migration, Advisory, Severity};
pub use approval::Approval;
pub use backend::{MigrationBackend, PostgresBackend, SessionSnapshot};
pub use backend_sqlite::{RebuildError, SqliteActorError, SqliteBackend, SqliteRebuildSpec};
pub use baseline::{BaselineError, BaselineOutcome};
pub use backfill::{
    backfill_progress, ensure_backfill_progress, list_backfills, run_backfill,
    run_backfill_bounded, BackfillError, BackfillOutcome, BackfillProgress, BackfillSpec,
};
pub use author::{
    AuthorError, AuthorRequest, Column, DeterministicAuthor, MigrationAuthor, RawSqlAuthor,
};
pub use classify::{
    classify, relations_touched, DdlKind, OwnershipNeed, ParseError, StatementClass,
    TouchedRelation,
};
pub use declarative::{
    desired_snapshot, desired_snapshot_for_dialect, dsl_to_pg_data_type, CollectionDescriptor,
    DeclarativeAuthor, DeclarativeError, DeclarativePlan, DesiredSchema, FieldDescriptor,
    IndexDescriptor, RenameHint, SqliteRebuild,
};
pub use engine::{
    DeclarativeApplyError, DeclarativeDeployOutcome, DeclarativeDeployPlan, EngineError,
    MigrationEngine, MigrationPlan, OnlineError, PlannedMigration, RollbackEngineError,
};
pub use expand_contract::{
    ExpandContractAuthor, ExpandContractError, ExpandContractPlan, OnlineIntent, OnlineSchemaChange,
    PgOnline,
};
pub use db::{connect, ConnectError, ExecutorConfig, PgConfinement};
pub use drift::{
    check_checksum_drift, diff_snapshots, snapshot_schema, AlteredObject, ChecksumDrift,
    ChecksumDriftReport, ColumnSnapshot, ConstraintSnapshot, DriftError, DriftReport, IndexSnapshot,
    OrphanJournal, SchemaSnapshot, StructuralDrift, TableSnapshot,
};
pub use executor::{
    apply, rollback, ApplyError, ApplyOutcome, LockMode, PreconditionVerdict, RollbackError,
    RollbackOptions, RollbackOutcome, RollbackRequest, RollbackTarget,
};
pub use guard::{
    flags_for, guard_for, GuardConfig, GuardError, GuardOutcome, GuardReport, MigrationGuard,
    PgGuard, SchemaScope, SqlGuard, SqliteDescriptorGuard, TrustProfile,
};
// The deploy-target dialect (§2.4.1) — re-exported so the control-plane deploy
// path can thread it into `IrAuthor::new` without depending on `zeroship-schema`.
pub use zeroship_schema::query::SqlDialect;
// `OperatorCapability` the TYPE is re-exported crate-wide so the `platform(...)`
// and `trusted(...)` constructors can name it in their signatures; its `new()`
// mint stays private to `guard::platform_runner` (design §4.1 / §5).
#[allow(unused_imports)]
pub(crate) use guard::OperatorCapability;
pub use journal::{
    applied, applied_count, ensure_journal, history as journal_history,
    latest_completed_checksums, net_rolled_back, record_baseline, record_completed,
    record_rolled_back, record_started, superseded_versions, AppliedEntry, HistoryEvent,
    HistoryKind, JournalError, JournaledKind, Phase, RolledBackEntry,
};
pub use loader::{
    load_dir, load_dir_migrations, migration_id_for_version, new_dbmate_migration,
    repeatable_id_for_name, LoaderError, PLATFORM_OWNER_APP,
};
pub use squash::{squash, SquashError, SquashOutcome};
pub use status::{history, status, MigrationStatus, StatusError};
pub use submit::{
    submit_migration, Submission, SubmissionOutcome, SubmitError,
};
pub use manifest::{
    compute_manifest, verify_manifest, ManifestError, ManifestHash, MismatchKind,
};
pub use migration::{
    Checksum, ChecksumInput, IdError, Migration, MigrationFlags, MigrationId, OnlinePhase,
    MIGRATION_PREFIX,
};
// The `op.*` portable IR (§2.1/§2.3/§2.5): the migration document, the closed
// `Op` enum, the constrained numeric scalar, and the canonical op-list the
// `Checksum::of_ir` front door folds. There is NO `Raw`/`RawDown` (property A);
// every transform/predicate is the closed [`expr::Expr`] AST.
pub use ir::{
    CanonicalOpList, ColType, IndexMethod, IrBatch, IrColumn, IrConstraint, IrConstraintKind,
    IrDefault, IrFlagsOverride, IrIndex, IrScalar, IrVersionError, MigrationIr, Op, SafeU64,
    SynthDefaultFn, CURRENT_IR_VERSION, EXPR_INVALID_NUMERIC,
};
// The fail-closed `.ir.json` load gate (§5.2/§5.3/§8.6): deserialize →
// `ir_version` → `validate_ir` → server-stamped ownership → advisory checksum-hint
// compare. The loader's IR branch ([`ir_author::IrAuthor::load_and_lower`]) runs
// this gate and then lowers the validated, owned IR to migrations (§7.2).
pub use ir_load::{
    enforce_ir_ownership, load_ir_document, recompute_hint_domain_checksum, IrLoadError,
};
// The IR-path DDL Lower phase (§6/§6.4/§6.5): compiles a validated, ownership-
// checked `MigrationIr` to migrations, reusing the SHARED snapshot-builder +
// declarative render seam so its SQL is byte-identical to the differ's path.
pub use ir_author::{
    FragmentGuardDenied, GuardedFragment, IrAuthor, IrGuardedLowerError, IrLowerError, LiveSchema,
    LoadAndLowerError, LoadAndLowerGuardedError, LoweredArtifact,
};
// The closed expression AST (§3.3.1) the IR's transform/predicate positions
// carry. Constructed in JS, serialized as data, NEVER parsed from text.
pub use expr::{BinaryOp, CaseBranch, CastTarget, Expr, ScalarFn, SynthFn, UnaryOp};
// The STRUCTURAL expression-AST validator + the structured-error envelope
// (§3.3.1.1 / §8.8). No parser, no fuzzer — a pure allow-list walk.
pub use validate::{
    validate_expr, validate_ir, validate_ir_resolved, validate_op, validate_op_resolved,
    AuthoringError, Dialect as ValidatorDialect, TargetScope, UnsupportedKind,
    CODE_DIALECT_SCOPE_PGONLY, CODE_EXPR_NOT_PORTABLE, CODE_OP_OUTSIDE_RECORDER, CODE_UNSUPPORTED,
    SPLIT_PART_MAX_N,
};
// The `op.*` DSL plan model (§2.0). Distinct from the dry-run `MigrationPlan`
// (re-exported from `engine`, unchanged): these are the net-new ordered
// EXECUTION artifact + its steps. `+AppliedPlan, +PlanStep, +RenameStep` added;
// `MigrationPlan` kept.
pub use plan::{
    AppliedPlan, BindValue, DialectScope, NotSingleStep, PlanStep, RenameStep,
};
pub use precondition::{
    evaluate as evaluate_precondition, CmpOp, OnUnmet, Precondition, PreconditionCheck,
    PreconditionError,
};
pub use role::{deprovision_migrator, migrator_role_name, provision_migrator, RoleError};
pub use shadow::{
    dry_run, dry_run_declarative, dry_run_incremental, sweep_leaked_shadows, DryRunError,
    DryRunReport, MigrationResult, PgShadow, ShadowConfig, ShadowDryRun,
};
#[doc(hidden)]
pub use shadow::arm_panic_after_provision;
