//! Optional backend capabilities shared by the generic apply orchestrator.

use crate::apply::drift::{DriftError, StructuralDrift};
use crate::conn::{ConnectError, ExecutorConfig};
use crate::engine::{DeclarativeDeployPlan, EngineError, OnlineError};
use crate::model::migration::{Migration, MigrationId};
use crate::render::declarative::{DeclarativeError, DesiredSchema};
use crate::render::expand_contract::OnlineIntent;
use zero_migrate_backend::advisory::Advisory;

// ── What a backfill IS, what running one produces, and how one refuses: all four
// now live with the backend contract, beside the `BackfillSpec` a vendor executor
// is handed. Re-exported so `capability::{BackfillSpec, BackfillOutcome,
// BackfillError}` still resolve.
pub use zero_migrate_backend::backfill::{BackfillError, BackfillOutcome, BackfillSpec};

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
