//! Optional backend capabilities shared by the generic apply orchestrator.

use crate::apply::drift::DriftError;
use crate::conn::{ConnectError, ExecutorConfig};
use crate::engine::{DeclarativeDeployPlan, EngineError};
use crate::model::migration::{Migration, MigrationId};
use crate::render::declarative::{DeclarativeError, DesiredSchema};
use crate::render::expand_contract::OnlineIntent;

// ── What a backfill IS, what running one produces, and how one refuses: all four
// now live with the backend contract, beside the `BackfillSpec` a vendor executor
// is handed. Re-exported so `capability::{BackfillSpec, BackfillOutcome,
// BackfillError}` still resolve.
pub use zero_migrate_backend::backfill::{BackfillError, BackfillOutcome, BackfillSpec};
// ── The two capability signatures' neutral halves: what an online expand is
// handed and refuses with, and the shadow dry-run's config + report. All five
// reached nothing above the backend contract, so they went down ahead of the
// traits. Re-exported so `capability::{OnlineError, ShadowConfig, DryRunReport,
// MigrationResult}` still resolve.
pub use zero_migrate_backend::capability::{
    DryRunReport, MigrationResult, OnlineError, ShadowConfig,
};

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
