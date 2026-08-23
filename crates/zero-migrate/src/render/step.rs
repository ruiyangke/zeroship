//! Low-level lowered-plan step values.
//!
//! Every one of them now lives in `zero-migrate-backend` and is re-exported from
//! here, the path each has always been reached by.
//!
//! This module used to be the last thing keeping `MigrationBackend` in the engine,
//! and the obstacle was never `PlanStep` itself. It was one FIELD, four types down:
//! `PlanStep::OnlineRename` carries a [`RenameStep`], whose `TableRebuild` arm
//! carries a [`TableRebuild`], whose [`TableRebuildSpec`] had a `sequence_policy`
//! typed `zero_migrate_sqlite::SqliteSequencePolicy` — a VENDOR type, in the shared
//! plan vocabulary, pointing the dependency the wrong way through the whole chain.
//! That field carries a neutral
//! [`SequenceHighWaterPolicy`](zero_migrate_backend::table_rebuild::SequenceHighWaterPolicy)
//! now, `zero-migrate-sqlite` converts it at its own boundary, and the chain
//! travelled.

/// A typed scalar bound into a parameterized [`PlanStep::Dml`] statement.
///
/// It is the currency of
/// [`DmlRenderer::bind_bytes`](zero_migrate_backend::renderer::DmlRenderer::bind_bytes),
/// so a vendor crate cannot implement the contract without naming it. It carries
/// nothing but scalars, so it was the first of this module to travel — alone, and
/// long before the rest could follow.
pub use zero_migrate_backend::step::BindValue;

/// The three steps that stay STRUCTURED until apply, because each one must read
/// the live catalog under the migration lock before it can spell its statement.
///
/// They are `MigrationBackend::{alter_primary_key, alter_column_type,
/// synchronize_identity}` arguments, so a vendor crate cannot implement the
/// contract without naming them. Each carries a
/// [`Migration`](crate::model::migration::Migration), an `AlterPrimaryKeyAction`
/// and `String`s, and nothing else.
pub use zero_migrate_backend::step::{
    AlterColumnTypeStep, AlterPrimaryKeyStep, SynchronizeIdentityStep,
};

/// The ordered execution vocabulary: what a lowered artifact's steps ARE, which
/// of two strategies a rename was lowered to, how far a plan's dialect reach
/// extends, and what rolling one step back can be relied on to do.
///
/// [`PlanStep`] is named by `MigrationBackend::rollback_plan_transactional`, and
/// [`TableRebuildSpec`] by `rebuild_one`, so the contract cannot be stated without
/// them.
pub use zero_migrate_backend::step::{
    tables_touched_by, DialectScope, PlanStep, RenameStep, StepReversibility,
};
/// The rebuild a `RenameStep::TableRebuild` carries, and its fully-resolved
/// execution spec. Re-exported here as well as from
/// [`crate::render::plan`]/[`crate::render::declarative`] because
/// [`RenameStep`] names both and a reader arrives at them through this module.
pub use zero_migrate_backend::table_rebuild::{TableRebuild, TableRebuildSpec};
