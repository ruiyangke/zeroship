//! Optional backend capabilities shared by the generic apply orchestrator.

use crate::apply::drift::DriftError;
use crate::conn::{ConnectError, ExecutorConfig};
use crate::engine::DeclarativeDeployPlan;
use crate::model::migration::Migration;
use crate::render::declarative::DesiredSchema;

// -- What a backfill IS, what running one produces, and how one refuses: all four
// now live with the backend contract, beside the `BackfillSpec` a vendor executor
// is handed. Re-exported so `capability::{BackfillSpec, BackfillOutcome,
// BackfillError}` still resolve.
pub use zeroship_migrate_backend::backfill::{BackfillError, BackfillOutcome, BackfillSpec};
// -- The online capability, whole: the trait, the `OnlineIntent` it is handed and
// the `OnlineError` it refuses with. Nothing in `run_online_backfill`'s signature
// reaches the engine any more, and - since the phases were inverted and the engine
// drives them - neither does its body, so the vendor that implements it no longer
// has to name the orchestrator.
// -- The shadow dry-run's neutral half travelled too: `ShadowConfig` in,
// `DryRunReport`/`MigrationResult` out. The `ShadowDryRun` TRAIT stayed, because
// `dry_run_declarative` takes a `DeclarativeDeployPlan` and a `DesiredSchema` -
// engine orchestration results, not vocabulary a backend speaks.
// Re-exported so every historical `capability::...` path still resolves.
pub use zeroship_migrate_backend::capability::{
    BackendCapability, DryRunReport, MigrationResult, OnlineError, OnlineSchemaChange, ShadowConfig,
};

/// A failure of the dry-run harness itself.
#[derive(Debug, thiserror::Error)]
pub enum DryRunError {
    /// Opening the second shadow session failed.
    #[error("connect to shadow db: {0}")]
    Connect(#[from] ConnectError),
    /// Introspecting the resulting shadow schema failed.
    #[error("snapshot shadow schema: {0}")]
    Drift(#[from] DriftError),
    /// The active backend has no shadow dry-run capability.
    #[error("shadow dry-run unsupported on this backend (no ShadowDryRun capability)")]
    ShadowUnsupported,
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
