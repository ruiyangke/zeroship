//! Optional backend capabilities shared by the generic apply orchestrator.

use crate::analysis::analyze::Advisory;
use crate::apply::drift::{DriftError, StructuralDrift};
use crate::apply::journal::JournalError;
use crate::apply::role::RoleError;
use crate::conn::{ConnectError, ExecutorConfig};
use crate::engine::{DeclarativeDeployPlan, EngineError, OnlineError};
use crate::guard::GuardError;
use crate::model::migration::{Migration, MigrationId};
use crate::render::declarative::{DeclarativeError, DesiredSchema};
use crate::render::expand_contract::OnlineIntent;

pub use crate::model::backfill::BackfillSpec;

/// What a Postgres backfill run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillOutcome {
    /// The backfill's stable id (the progress-row key).
    pub backfill_id: String,
    /// Number of batches committed **this run**.
    pub batches: u64,
    /// Number of rows updated **this run**.
    pub rows_updated: u64,
    /// `true` if a prior progress row was found and this run resumed.
    pub resumed: bool,
    /// `true` when the backfill reached the end of the table.
    pub complete: bool,
}

/// Error from a backend backfill executor.
#[derive(Debug, thiserror::Error)]
pub enum BackfillError {
    /// A progress / journal-schema operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// A test-only injected fault at a batch boundary.
    #[error("backfill fault-injection: {0}")]
    Fault(String),
    /// A backfill mutates table data and requires explicit approval.
    #[error("backfill requires Approval::Approved (it mutates table data) but it was not given")]
    ApprovalRequired,
    /// A stable backfill step was resumed with different authored content.
    #[error("checksum drift on backfill {version}: progress has {recorded}, plan has {expected}")]
    ChecksumDrift {
        /// The stable plan-step version.
        version: String,
        /// The checksum stored with the progress row.
        recorded: String,
        /// The checksum supplied by the current plan.
        expected: String,
    },
    /// A bare SQL identifier in the spec is invalid.
    #[error("invalid identifier for {what}: {value:?} (must be a bare [A-Za-z_][A-Za-z0-9_]* identifier)")]
    InvalidIdentifier {
        /// Which field was invalid.
        what: &'static str,
        /// The offending value.
        value: String,
    },
    /// A structured backfill specification is internally inconsistent.
    #[error("invalid backfill specification: {0}")]
    InvalidSpec(String),
    /// [`BackfillSpec::batch_size`] was zero.
    #[error("batch_size must be non-zero")]
    InvalidBatchSize,
    /// The assembled statement was denied by the SQL guard.
    #[error("backfill assembled statement denied by guard: {source}")]
    Guard {
        /// The underlying guard rejection.
        #[source]
        source: GuardError,
    },
    /// The target table or one of the cursor components cannot be resolved.
    #[error("backfill target not found: {0}")]
    TargetNotFound(String),
    /// The ordered cursor tuple is not a usable unique, non-null paging key.
    #[error(
        "cursorColumns {cursor_columns:?} on {table:?} cannot provide a stable resumable \
         backfill cursor ({reason}); choose a one-shot update under a maintenance window, a \
         target-specific rebuild or temporary surrogate, or creation of a stable unique cursor \
         in an earlier migration"
    )]
    CursorTupleUnavailable {
        /// The target table.
        table: String,
        /// The offending ordered cursor tuple.
        cursor_columns: Vec<String>,
        /// Why it was rejected.
        reason: String,
    },
    /// The authored transform mutates a cursor component.
    #[error(
        "backfill transform assigns cursor component {cursor_component:?}; no cursorColumns \
         component may change while the operation pages"
    )]
    CursorComponentMutated {
        /// The cursor component the transform illegally assigns.
        cursor_component: String,
    },
    /// A SQLite backfill batch failure after the last committed cursor.
    #[error("sqlite backfill batch failed at cursor {at_cursor:?}: {source_msg}")]
    SqliteBatchFailed {
        /// The last committed cursor when the failing batch started.
        at_cursor: Option<String>,
        /// The SQLite error message from the failed batch.
        source_msg: String,
    },
    /// The SQLite migration connection can no longer be safely reused.
    #[error("sqlite backfill connection poisoned: {0}")]
    SqlitePoisoned(String),
}

/// The online schema-change capability — the dialect-neutral seam the generic
/// declarative apply path uses to drive a zero-downtime online operation.
#[allow(clippy::module_name_repetitions)]
pub trait OnlineSchemaChange {
    /// Drive one online intent's expand sequence.
    #[allow(clippy::too_many_arguments)]
    fn run_online<'a>(
        &'a self,
        intent: &'a OnlineIntent,
        expand: &'a [Migration],
        backfill: &'a BackfillSpec,
        approval: crate::approval::Approval,
        scope: &'a crate::approval::ApprovalScope,
        trigger_version: &'a MigrationId,
        cfg: &'a ExecutorConfig,
        applied_by: &'a str,
        lock_mode: crate::apply::executor::LockMode,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::apply::executor::ApplyOutcome, OnlineError>,
                > + 'a,
        >,
    >;
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

/// A failure of the dry-run harness itself.
#[derive(Debug, thiserror::Error)]
pub enum DryRunError {
    /// Opening the second shadow session failed.
    #[error("connect to shadow db: {0}")]
    Connect(#[from] ConnectError),
    /// Provisioning the shadow schema or migrator role failed.
    #[error("provision shadow: {0}")]
    Provision(#[from] RoleError),
    /// Introspecting the resulting shadow schema failed.
    #[error("snapshot shadow schema: {0}")]
    Drift(#[from] DriftError),
    /// Seeding the shadow with the current live project schema failed.
    #[error("seed shadow from live schema: {0}")]
    Seed(#[source] SeedError),
    /// The active backend has no shadow dry-run capability.
    #[error("shadow dry-run unsupported on this backend (no ShadowDryRun capability)")]
    ShadowUnsupported,
}

/// A failure to reconstruct the live project structure inside the fresh shadow.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    /// The declarative author could not turn the live snapshot into migrations.
    #[error("author seed migrations from live snapshot: {0}")]
    Author(#[from] DeclarativeError),
    /// Applying the reconstruction migrations on the shadow failed.
    #[error("apply seed migrations on shadow: {0}")]
    Apply(#[from] EngineError),
}

/// The per-engine shadow dry-run capability.
#[allow(clippy::module_name_repetitions)]
pub trait ShadowDryRun {
    /// Dry-run a migration batch against a throwaway shadow clone.
    fn dry_run<'a>(
        &'a self,
        migrations: &'a [Migration],
        cfg: &'a ExecutorConfig,
        shadow_cfg: &'a ShadowConfig,
        applied_by: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<DryRunReport, DryRunError>> + 'a>>;

    /// Dry-run a declarative deploy plan against a seeded shadow clone.
    fn dry_run_declarative<'a>(
        &'a self,
        plan: &'a DeclarativeDeployPlan,
        desired: &'a DesiredSchema,
        cfg: &'a ExecutorConfig,
        shadow_cfg: &'a ShadowConfig,
        applied_by: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<DryRunReport, DryRunError>> + 'a>>;
}
