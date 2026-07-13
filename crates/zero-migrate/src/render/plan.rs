//! The applied-execution plan model (`op.*` DSL) and the single
//! shared plan orchestrator's data types.
//!
//! One authored migration artifact — a `.sql` file *or* an
//! IR envelope — lowers to an [`AppliedPlan`]: an ordered sequence of
//! [`PlanStep`]s the engine's single shared `apply_plan`
//! ([`MigrationEngine::apply_plan`](crate::engine::MigrationEngine::apply_plan))
//! runs in order. The step *types* reuse the engine's existing phase artifacts
//! ([`Migration`], [`BackfillSpec`](crate::model::backfill::BackfillSpec),
//! [`ExpandContractPlan`](crate::render::expand_contract::ExpandContractPlan), and the
//! existing [`declarative::SqliteRebuild`](crate::render::declarative::SqliteRebuild)) —
//! this introduces **no** new rebuild struct and **no** change to [`Migration`].
//!
//! # Naming — a deliberate collision avoidance
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
//! plan whose `steps == [Ddl(one Migration)]` — the overwhelming common case,
//! and the only shape the legacy Flyway/dbmate loader ever produces. The
//! [`AppliedPlan::single_step`] facade builds exactly that, and
//! [`AppliedPlan::single_step_migration`] reads it back out (fail-closed on a
//! multi-step plan).

use crate::model::migration::{Checksum, Migration, MigrationFlags, MigrationId};
use crate::model::precondition::PreconditionCheck;
use crate::render::step::{DialectScope, PlanStep};

/// The fully-resolved specification for ONE table rebuild.
#[derive(Debug, Clone)]
pub struct SqliteRebuildSpec {
    /// The existing table being rebuilt (the final name; the new table is renamed
    /// INTO this).
    pub table: String,
    /// The temp name the new table is created under, then renamed FROM.
    pub tmp_table: String,
    /// The new table's `CREATE TABLE <tmp> (...)` DDL.
    pub new_table_create: String,
    /// The columns to copy from the old table into the new one, as `(dest, src)`
    /// pairs of BARE identifiers.
    pub copy_columns: Vec<(String, String)>,
    /// EXTRA dependent DDL to replay AFTER the rename.
    pub recreate_objects: Vec<String>,
    /// BARE names of columns being DROPPED by this rebuild.
    pub dropped_columns: Vec<String>,
    /// A human-readable description of what change drove the rebuild.
    pub reason: String,
}

impl SqliteRebuildSpec {
    /// The engine-chosen temp-table name for `table`.
    #[must_use]
    pub fn tmp_name(table: &str) -> String {
        format!("{table}__zero_migrate_rebuild")
    }
}

/// What one authored artifact (`.sql` or IR envelope) becomes after
/// lowering — an ordered execution plan. NOT a single [`Migration`]; NOT
/// the dry-run [`MigrationPlan`](crate::engine::MigrationPlan).
#[derive(Debug, Clone)]
pub struct AppliedPlan {
    /// The outer plan version (the filename's `<NNNN>` → deterministic UUIDv7).
    pub version: MigrationId,
    /// Human-readable name.
    pub name: String,
    /// Ordered steps; `apply_plan` runs them in sequence.
    pub steps: Vec<PlanStep>,
    /// ONE checksum over the canonical artifact (for a `.sql` plan this is the
    /// single step's `Migration.checksum`; for an IR envelope it is
    /// `Checksum::of_ir` over the op list).
    pub checksum: Checksum,
    /// Flags derived ∪ overridden from the artifact.
    pub flags: MigrationFlags,
    /// The plan's dialect reach; a separate journaled facet, not folded
    /// into the checksum.
    pub dialect_scope: DialectScope,
    /// `false` if ANY step is `down: None` (Backfill/Dml/incomplete OnlineRename);
    /// surfaced by status/rollback BEFORE attempt.
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
/// the platform path (a Flyway `.sql` always lowers to one `Ddl` step) — it
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
    /// Build the **degenerate one-step plan** for a single pure-DDL [`Migration`]
    /// — the loader facade for a `.sql` file. The plan's identity
    /// fields mirror the migration; `dialect_scope` is `Both` (a `.sql` plan is
    /// not `op.raw`-pinned), and `rollbackable` follows the migration's `down`.
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
            checksum,
            flags,
            dialect_scope: DialectScope::Both,
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
}
