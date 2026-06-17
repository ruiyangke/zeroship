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
pub mod backfill;
pub mod baseline;
pub mod classify;
pub mod db;
pub mod declarative;
pub mod drift;
pub mod engine;
pub mod expand_contract;
pub mod executor;
pub mod guard;
pub mod journal;
pub mod migration;
pub mod precondition;
pub mod role;
pub mod shadow;
pub mod squash;
pub mod status;

// ---------------------------------------------------------------------------
// Public API surface — re-exports (later plans depend on these names).
// ---------------------------------------------------------------------------

pub use analyze::{analyze, analyze_migration, Advisory, Severity};
pub use approval::Approval;
pub use baseline::{baseline, BaselineError, BaselineOutcome};
pub use backfill::{
    backfill_progress, ensure_backfill_progress, list_backfills, run_backfill,
    run_backfill_bounded, BackfillError, BackfillOutcome, BackfillProgress, BackfillSpec,
};
pub use author::{
    AuthorError, AuthorRequest, Column, DeterministicAuthor, MigrationAuthor, RawSqlAuthor,
};
pub use classify::{classify, DdlKind, ParseError, StatementClass};
pub use declarative::{
    desired_snapshot, dsl_to_pg_data_type, CollectionDescriptor, DeclarativeAuthor,
    DeclarativeError, DeclarativePlan, DesiredSchema, FieldDescriptor, IndexDescriptor, RenameHint,
};
pub use engine::{
    DeclarativeApplyError, DeclarativeDeployOutcome, DeclarativeDeployPlan, EngineError,
    MigrationEngine, MigrationPlan, OnlineError, PlannedMigration, RollbackEngineError,
};
pub use expand_contract::{
    ExpandContractAuthor, ExpandContractError, ExpandContractPlan, OnlineIntent,
};
pub use db::{connect, ConnectError, ExecutorConfig};
pub use drift::{
    check_checksum_drift, diff_snapshots, snapshot_schema, AlteredObject, ChecksumDrift,
    ChecksumDriftReport, ColumnSnapshot, ConstraintSnapshot, DriftError, DriftReport, IndexSnapshot,
    OrphanJournal, SchemaSnapshot, StructuralDrift, TableSnapshot,
};
pub use executor::{
    apply, rollback, ApplyError, ApplyOutcome, RollbackError, RollbackOptions, RollbackOutcome,
    RollbackRequest, RollbackTarget,
};
pub use guard::{flags_for, GuardConfig, GuardError, GuardReport, SqlGuard};
pub use journal::{
    applied, applied_count, ensure_journal, history as journal_history,
    latest_completed_checksums, net_rolled_back, record_baseline, record_completed,
    record_rolled_back, record_started, superseded_versions, AppliedEntry, HistoryEvent,
    HistoryKind, JournalError, JournaledKind, Phase, RolledBackEntry,
};
pub use squash::{squash, SquashError, SquashOutcome};
pub use status::{history, status, MigrationStatus, StatusError};
pub use migration::{
    Checksum, IdError, Migration, MigrationFlags, MigrationId, OnlinePhase, MIGRATION_PREFIX,
};
pub use precondition::{
    evaluate as evaluate_precondition, CmpOp, OnUnmet, Precondition, PreconditionCheck,
    PreconditionError,
};
pub use role::{deprovision_migrator, migrator_role_name, provision_migrator, RoleError};
pub use shadow::{
    dry_run, dry_run_declarative, sweep_leaked_shadows, DryRunError, DryRunReport, MigrationResult,
    ShadowConfig,
};
#[doc(hidden)]
pub use shadow::arm_panic_after_provision;
