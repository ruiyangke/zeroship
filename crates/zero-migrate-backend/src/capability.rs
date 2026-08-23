//! Optional backend capabilities' shared vocabulary.
//!
//! The two capability TRAITS — `OnlineSchemaChange` and `ShadowDryRun` — are
//! still `zero_migrate::apply::backend::capability`. What lives here is the part
//! of their signatures that reaches nothing above this crate: the online
//! [`OnlineIntent`] a backend is handed and the [`OnlineError`] it refuses with,
//! and the shadow dry-run's [`ShadowConfig`] input plus its
//! [`DryRunReport`]/[`MigrationResult`] output.
//!
//! What held the traits back is named where each blocker sits:
//! `ShadowDryRun::dry_run_declarative` takes a `DeclarativeDeployPlan` and a
//! `DesiredSchema`, and `SeedError` carries an `EngineError` — all three are
//! engine orchestration results, not backend vocabulary.
//!
//! The engine re-exports every item here under its historical path.

use crate::advisory::Advisory;
use crate::drift::StructuralDrift;
use crate::executor::ApplyError;

/// A high-level online-migration intent the engine's `ExpandContractAuthor`
/// expands into an ordered, phased
/// [`Migration`](zero_migrate_ir::migration::Migration) sequence.
///
/// It is the NEUTRAL half of that authoring: `ExpandContractPlan` carries the
/// authored PostgreSQL DDL, and this carries the intent behind it, so the generic
/// declarative apply path can hand a backend what the rename MEANS rather than one
/// vendor's spelling of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnlineIntent {
    /// Rename column `from` → `to` (of type `ty`) on `table`, online, via the
    /// canonical expand-contract dual-write sequence.
    RenameColumn {
        /// The table the column lives on (bare; project-schema-qualified on emit).
        table: String,
        /// The existing column name.
        from: String,
        /// The new column name.
        to: String,
        /// The Postgres type of the column (emitted verbatim for the new column).
        ty: String,
    },
}

/// A failure from `OnlineSchemaChange::run_online`.
#[derive(Debug, thiserror::Error)]
pub enum OnlineError {
    /// The online expand needs explicit [`Approval::Approved`](crate::approval::Approval::Approved) (its backfill
    /// mutates data). Nothing was applied.
    #[error("online expand requires approval (the backfill mutates data) but none was given")]
    Approval,
    /// **Per-version approval scope (executor-layer defense in depth).** The
    /// expand is approved ([`Approval::Approved`](crate::approval::Approval::Approved)) but the rename's PLAN-GROUP
    /// version is NOT in the operator's reviewed
    /// [`ApprovalScope::Versions`](crate::approval::ApprovalScope::Versions) set — the
    /// executor-layer mirror of the engine's EXPAND scope gate, so a direct
    /// `run_online` / `run_expand_pg` caller cannot mirror data for a rename the
    /// operator never individually reviewed. Nothing was applied.
    #[error(
        "online expand for version '{version}' is not in the approved version scope \
         (per-version approval required)"
    )]
    ApprovalNotScoped {
        /// The rename's PLAN-GROUP version the scope refused.
        version: String,
    },
    /// Applying E1/E2 or the E3 backfill marker failed.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// The backfill step failed — E3 is NOT journaled, so the gate keeps the
    /// expand incomplete (the contract stays blocked) and the backfill is
    /// resumable on a re-run.
    #[error(transparent)]
    Backfill(#[from] crate::backfill::BackfillError),
}

/// Where + how to provision a throwaway shadow database.
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    /// A DSN for an admin connection whose role has `CREATEDB`.
    pub admin_dsn: String,
    /// The prefix for the throwaway database name.
    pub db_name_prefix: String,
}

/// The per-migration outcome of a dry-run apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationResult {
    /// The migration's version (`mig_...`).
    pub version: String,
    /// Whether this migration's `up` applied cleanly on the shadow.
    pub applied_ok: bool,
    /// The error when `applied_ok == false`.
    pub error: Option<String>,
}

/// The result of a dry-run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunReport {
    /// Overall success.
    pub ok: bool,
    /// Per-migration outcome, in apply order.
    pub per_migration: Vec<MigrationResult>,
    /// Declarative resulting drift, if this was a declarative dry-run.
    pub resulting_drift: Option<StructuralDrift>,
    /// Operational advisories per migration version.
    pub advisories: Vec<(String, Vec<Advisory>)>,
    /// Whether the shadow DB + role teardown fully succeeded.
    pub teardown_ok: bool,
    /// The teardown failure message when `teardown_ok == false`.
    pub teardown_error: Option<String>,
}
