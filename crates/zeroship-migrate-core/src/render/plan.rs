//! The applied-execution plan model (`op.*` DSL) and the single
//! shared plan orchestrator's data types.
//!
//! One authored migration artifact - a `.sql` file *or* an
//! IR envelope - lowers to an [`AppliedPlan`]: an ordered sequence of
//! [`PlanStep`]s the engine's single shared `apply_plan`
//! ([`MigrationEngine::apply_plan`](crate::engine::MigrationEngine::apply_plan))
//! runs in order. The step *types* reuse the engine's existing phase artifacts
//! ([`Migration`], [`BackfillSpec`](crate::model::backfill::BackfillSpec),
//! [`ExpandContractPlan`](crate::render::expand_contract::ExpandContractPlan), and the
//! existing [`declarative::TableRebuild`](crate::render::declarative::TableRebuild)) -
//! this introduces **no** new rebuild struct and **no** change to [`Migration`].
//!
//! # Naming - a deliberate collision avoidance
//!
//! This is **`AppliedPlan`**, NOT `MigrationPlan`. `MigrationPlan`
//! ([`engine::MigrationPlan`](crate::engine::MigrationPlan)) is the read-only
//! lint/dry-run **preview** result and keeps its name and meaning, untouched.
//! `AppliedPlan` is the net-new *ordered execution artifact*. The two coexist on
//! the public surface as distinct symbols.
//!
//! # The single-`Migration` case is the degenerate one-step plan
//!
//! A pure-DDL `.sql` (or IR envelope with no DML/backfill/online op) lowers to a
//! plan whose `steps == [Ddl(one Migration)]` - the overwhelming common case,
//! and the only shape the legacy Flyway/dbmate loader ever produces. The
//! [`AppliedPlan::single_step`] facade builds exactly that, and
//! [`AppliedPlan::single_step_migration`] reads it back out (fail-closed on a
//! multi-step plan).

use crate::model::migration::{Checksum, Migration, MigrationFlags, MigrationId};
use crate::model::precondition::PreconditionCheck;
use crate::render::step::{DialectScope, PlanStep, StepReversibility};
use zeroship_migrate_ir::dialect::DialectId;

// What a lowered plan needs the LIVE target to be able to do. Both types moved
// down to the backend contract, beside the
// `MigrationBackend::verify_database_requirements` signature that is the only
// thing that ASKS the question; the engine only collects the answer while
// lowering. They travelled alone - `DatabaseFeature` is a closed enum of
// `&'static str` descriptions and version floors, and `DatabaseRequirements` is a
// `BTreeSet` of it. Re-exported so `crate::render::plan::{DatabaseFeature,
// DatabaseRequirements}` resolve unchanged.
pub use zeroship_migrate_backend::requirements::{DatabaseFeature, DatabaseRequirements};
// The fully-resolved specification for ONE table rebuild, and the neutral
// high-water policy that finally let it travel. Its `sequence_policy` used to be
// typed `zeroship_migrate_sqlite::SqliteSequencePolicy` - a type from a crate ABOVE
// the contract - which stranded this spec, `TableRebuild`, `RenameStep` and
// `PlanStep` in the engine for want of one field. Re-exported so
// `crate::render::plan::TableRebuildSpec` resolves unchanged.
pub use zeroship_migrate_backend::table_rebuild::{SequenceHighWaterPolicy, TableRebuildSpec};

/// The independent facts a caller needs before offering an operator a rollback.
///
/// A product rather than one ranked verdict, because the three are not on one
/// axis: whether evidence covers a step and how bad that step's outcome is are
/// different questions, and a plan can carry all three at once. Any total order
/// over them reports the winner and silently drops the rest - ranking an
/// unassessed step above a known-lossy one overstates the opaque, and ranking it
/// below hides that something was never measured.
///
/// Every field folds with OR across the plan's steps, so each answers "is there
/// at least one such step". A plan with no steps carries none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RollbackAssessment {
    /// At least one step has no `down`, so a complete rollback is unavailable
    /// however the rest of the plan is treated.
    pub has_irreversible_steps: bool,
    /// At least one step's `down` restores structure while the data it removed
    /// stays gone.
    pub has_known_lossy_steps: bool,
    /// At least one step's effect on the prior state is not established either
    /// way, so the harm of rolling back cannot be bounded from the plan alone.
    pub has_unassessed_steps: bool,
}

/// What one authored artifact (`.sql` or IR envelope) becomes after
/// lowering - an ordered execution plan. NOT a single [`Migration`]; NOT
/// the dry-run [`MigrationPlan`](crate::engine::MigrationPlan).
#[derive(Debug, Clone)]
pub struct AppliedPlan {
    /// The outer plan version. SQL files derive it from their numeric version;
    /// host IR derives it from server-stamped owner plus migration name.
    pub version: MigrationId,
    /// Human-readable name.
    pub name: String,
    /// Ordered steps; `apply_plan` runs them in sequence.
    pub steps: Vec<PlanStep>,
    /// Database features the typed IR requires the connected target to support.
    /// Apply checks this whole-plan set before executing any authored step.
    pub database_requirements: DatabaseRequirements,
    /// ONE checksum over the canonical artifact (for a `.sql` plan this is the
    /// single step's `Migration.checksum`; for an IR envelope it is
    /// `Checksum::of_ir` over the op list).
    pub checksum: Checksum,
    /// Flags derived from the artifact, unioned with those it overrides.
    pub flags: MigrationFlags,
    /// The plan's dialect reach, MEASURED from the op list at lowering - never
    /// authored, and not folded into the checksum.
    ///
    /// Apply consults it whole-plan before the project lock and before any step runs:
    /// a plan whose ops one registered backend alone can render is refused against
    /// every other target with
    /// [`EngineError::DialectScopeRefused`](crate::engine::EngineError::DialectScopeRefused).
    /// A `.sql` plan is always [`DialectScope::Portable`] - its text is opaque to the
    /// engine, so there is nothing to measure.
    pub dialect_scope: DialectScope,
    /// The backend that RENDERED these steps, when the engine rendered them.
    ///
    /// [`Self::dialect_scope`] answers which backends COULD render this plan's ops;
    /// this answers which one DID. They are different questions and only the first was
    /// ever compared against the deploy target. For a bare `createTable` the reach is
    /// honestly `Portable`, so the reach gate admits every target - while the SQL in
    /// the plan is one vendor's spelling: PostgreSQL lowers the op to
    /// `"main"."notes"`, SQLite to `main.notes`.
    ///
    /// The gap was not theoretical and not loud. SQLite accepts double-quoted
    /// identifiers and calls its own database `main`, so a PostgreSQL-rendered plan
    /// APPLIED CLEANLY against a SQLite target rather than failing - measured, not
    /// predicted. A MySQL target would have rejected the quoting and made it obvious;
    /// the quiet direction is the one that needed the gate.
    ///
    /// `None` for a `.sql` plan, and that is honest rather than a hole: the engine did
    /// not render that text, so it has no rendering backend to name. Such a plan is
    /// still bounded by `dialect_scope`, which is all the engine can say about an
    /// artifact whose contents are opaque to it.
    ///
    /// Not folded into the checksum, for the same reason `dialect_scope` is not: it is
    /// measured at lowering rather than authored, so folding it in would make one
    /// artifact checksum differently per target.
    pub rendered_for: Option<DialectId>,
    /// `false` if ANY step is `down: None` (Backfill/Dml/incomplete OnlineRename);
    /// surfaced by status/rollback BEFORE attempt.
    ///
    /// This answers whether reversing SQL EXISTS, not whether the original state
    /// can be restored. A dropped column is structurally reversible and its values
    /// are gone for good, and this still reports `true` - see
    /// `tests/plan_rollbackable.rs`, which pins both readings. A host presenting
    /// this to an operator as "safe to undo" is over-reading it.
    ///
    /// [`rollback_assessment`](Self::rollback_assessment) answers the question a
    /// host actually wants; this field is kept for callers already reading it.
    pub rollbackable: bool,
    /// The declaring app (server-stamped on the IR path).
    pub owner_app: String,
    /// Cross-plan ordering deps (attach to the first step).
    pub depends_on: Vec<MigrationId>,
    /// Squash supersession identity.
    pub supersedes: Vec<MigrationId>,
    /// Preconditions evaluated before the plan's first step.
    pub preconditions: Vec<PreconditionCheck>,
}

/// The fail-closed error of [`AppliedPlan::single_step_migration`]: the
/// plan is not a single `Ddl` step, so a `Migration`-only consumer (the platform
/// Flyway-mode runner) cannot operate on it. This arm is provably unreachable on
/// the platform path (a Flyway `.sql` always lowers to one `Ddl` step) - it
/// exists for defense in depth.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "migration plan {version} is not a single-step DDL plan (has {step_count} step(s)); \
     a Migration-only consumer cannot operate on a multi-step plan"
)]
pub struct NotSingleStep {
    /// The plan version that failed the single-step precondition.
    pub version: String,
    /// How many steps the plan actually has.
    pub step_count: usize,
}

impl AppliedPlan {
    /// Build the **degenerate one-step plan** for a single pure-DDL [`Migration`] -
    /// the loader facade for a `.sql` file. The plan's identity
    /// fields mirror the migration; `dialect_scope` is
    /// [`DialectScope::Portable`] and `rollbackable` follows the migration's `down`.
    ///
    /// `Portable` here is the ONLY answer this constructor can honestly give, and it
    /// is not a claim that the file's SQL runs anywhere: a `.sql` migration is opaque
    /// text with no op list to measure, so there is no reach to derive. Pinning it
    /// would invent a fact; refusing it would break every `.sql` deploy. The
    /// engine-authored IR path is where the reach is real.
    #[must_use]
    pub fn single_step(migration: Migration) -> Self {
        let version = migration.version.clone();
        let name = migration.name.clone();
        let checksum = migration.checksum.clone();
        let flags = migration.flags; // MigrationFlags is Copy
        let owner_app = migration.owner_app.clone();
        let depends_on = migration.depends_on.clone();
        let supersedes = migration.supersedes.clone();
        let preconditions = migration.preconditions.clone();
        let rollbackable = migration.down.is_some();
        AppliedPlan {
            version,
            name,
            steps: vec![PlanStep::Ddl(migration)],
            database_requirements: DatabaseRequirements::default(),
            checksum,
            flags,
            dialect_scope: DialectScope::Portable,
            // A `.sql` plan's text is the author's, not the engine's, so there is no
            // rendering backend to name. `dialect_scope` above is the whole of what
            // the engine can say about an artifact it cannot read.
            rendered_for: None,
            rollbackable,
            owner_app,
            depends_on,
            supersedes,
            preconditions,
        }
    }

    /// The thin `Migration`-facade the legacy SQL runner consumes:
    /// a plan whose `steps == [Ddl(_)]` yields that one `&Migration`;
    /// any other shape fails closed with [`NotSingleStep`]. This keeps the
    /// SQL runner operating over [`Migration`] and decoupled from
    /// `PlanStep`/`RenameStep` evolution.
    ///
    /// # Errors
    /// [`NotSingleStep`] if the plan is not exactly one `Ddl` step.
    pub fn single_step_migration(&self) -> Result<&Migration, NotSingleStep> {
        match self.steps.as_slice() {
            [PlanStep::Ddl(m)] => Ok(m),
            other => Err(NotSingleStep {
                version: self.version.as_str().to_string(),
                step_count: other.len(),
            }),
        }
    }

    /// True iff the plan is a single `Ddl` step (the platform-path precondition).
    /// Convenience over [`single_step_migration`](Self::single_step_migration).
    #[must_use]
    pub fn is_single_step(&self) -> bool {
        matches!(self.steps.as_slice(), [PlanStep::Ddl(_)])
    }

    /// Recompute `rollbackable` from the current steps: `true` iff every
    /// step has a defined `down`. Used by adapters that assemble `steps`
    /// directly.
    #[must_use]
    pub fn compute_rollbackable(steps: &[PlanStep]) -> bool {
        steps.iter().all(PlanStep::has_down)
    }

    /// What rolling this plan back can be relied on to do, as three independent
    /// facts folded with OR over the steps.
    ///
    /// Answers what [`rollbackable`](Self::rollbackable) is routinely misread as
    /// answering. A plan whose only step drops a column reports
    /// `rollbackable == true` and `has_known_lossy_steps == true` at the same
    /// time, and both are correct: the reversing SQL exists and the values do
    /// not come back.
    #[must_use]
    pub fn rollback_assessment(&self) -> RollbackAssessment {
        let mut facts = RollbackAssessment::default();
        for step in &self.steps {
            match step.reversibility() {
                StepReversibility::Irreversible => facts.has_irreversible_steps = true,
                StepReversibility::StructurallyReversibleLossy => {
                    facts.has_known_lossy_steps = true;
                }
                StepReversibility::Unassessed => facts.has_unassessed_steps = true,
            }
        }
        facts
    }
}
