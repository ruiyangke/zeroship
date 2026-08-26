//! The lowered-plan step VALUES that cross the backend boundary.
//!
//! [`BindValue`] is here because it is the currency of
//! [`DmlRenderer::bind_bytes`](crate::renderer::DmlRenderer::bind_bytes) - the
//! vendors disagree about the CARRIER of a binary value, which is a spelling
//! decision, so each one answers it.
//!
//! The three STRUCTURED steps are here because a backend is what executes them.
//! [`AlterPrimaryKeyStep`], [`AlterColumnTypeStep`] and
//! [`SynchronizeIdentityStep`] deliberately stay structured until apply - each
//! must read the live catalog under the migration lock before it can spell its
//! statement - so they are `MigrationBackend` arguments, not rendered DDL. Each
//! carries a [`Migration`], an [`AlterPrimaryKeyAction`] and `String`s, and
//! nothing else.
//!
//! # What is still in the engine, and the ONE thing holding it
//!
//! `zero_migrate::render::step` keeps `PlanStep`, `RenameStep` and
//! `DialectScope`. `PlanStep::OnlineRename` carries a `RenameStep`, which carries
//! `render::declarative::TableRebuild`, which carries
//! `render::plan::TableRebuildSpec` - and that spec's `sequence_policy` field is
//! typed `zero_migrate_sqlite::SqliteSequencePolicy`. A vendor type cannot come
//! DOWN into the contract crate the vendors sit above, so the chain stops there
//! rather than at anything about `PlanStep` itself.
//!
//! Every item here is re-exported from `zero_migrate::render::step`, the path
//! every existing caller uses.

use crate::backfill::BackfillSpec;
use crate::capability::{BackendCapability, ExpandContractPlan, OnlineIntent};
use crate::table_rebuild::TableRebuild;
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::ir::AlterPrimaryKeyAction;
use zero_migrate_ir::migration::Migration;
use zero_migrate_ir::migration::{Checksum, MigrationId};

/// A typed scalar bound into a parameterized `PlanStep::Dml` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindValue {
    /// SQL `NULL`.
    Null,
    /// A boolean.
    Bool(bool),
    /// An exact 64-bit integer (the only integer domain the IR admits).
    Int(i64),
    /// A decimal/float carried as its canonical string form (numeric
    /// domain: no `f64` in the IR identity).
    Decimal(String),
    /// A UTF-8 text value.
    Text(String),
    /// Exact binary bytes. SQLite binds this variant directly as a BLOB. The
    /// PostgreSQL and MySQL renderers use a text bind wrapped in the dialect's
    /// base64 decoder because their schema-blind host seams infer text values.
    Bytes(Vec<u8>),
}

/// One explicit primary-key lifecycle mutation.
///
/// Unlike ordinary rendered DDL, this remains structured until apply so the
/// backend can verify the exact live primary key, identity facets, candidate
/// uniqueness, and inbound foreign keys while it holds the migration lock.
#[derive(Debug, Clone)]
pub struct AlterPrimaryKeyStep {
    /// Journal marker and approval metadata for this operation.
    pub migration: Migration,
    /// Effective target schema selected during lowering.
    pub schema: String,
    /// Bare target table name.
    pub table: String,
    /// Exact add/replace/drop contract authored in the IR.
    pub action: AlterPrimaryKeyAction,
}

/// One column retype that the target dialect spells by RESTATING the column.
///
/// MySQL has no `ALTER COLUMN ... TYPE`. It has `MODIFY COLUMN`, which takes the
/// COMPLETE column definition and silently DISCARDS every facet the statement
/// leaves out - measured in `tests/mysql_engine/mysql_setcolumntype_restate.rs`,
/// where a bare `MODIFY COLUMN label varchar(128)` destroys the column's
/// `NOT NULL`, its `DEFAULT`, its `COLLATE` and its `COMMENT` in one statement,
/// with no warning.
///
/// So the statement cannot be written until the current definition is known, and
/// the current definition is not in the op: `Op::SetColumnType` carries one field.
/// This step stays STRUCTURED until apply for exactly the reason
/// [`AlterPrimaryKeyStep`] does - the backend reads `SHOW CREATE TABLE` under the
/// migration lock and restates the clause the server itself reports, so the answer
/// cannot go stale between the read and the `ALTER`.
///
/// WHY NOT AT LOWER TIME, which would need no new step at all: the apply path DOES
/// have the live column there (the engine's `LiveSchema::table_snapshots` is populated
/// from a real catalog read by `engine.rs` before it lowers). But
/// `ColumnSnapshot` is a LOSSY projection of a MySQL column - the same test
/// measures that `COLUMN_COMMENT` is never read, that `EXTRA` is read only for
/// `auto_increment` / `DEFAULT_GENERATED` so `ON UPDATE CURRENT_TIMESTAMP` cannot
/// be spelled, and that the generated-column facet is deliberately left `None`.
/// Restating from it would drop those four facets SILENTLY, which is strictly
/// worse than today's refusal.
#[derive(Debug, Clone)]
pub struct AlterColumnTypeStep {
    /// Journal marker and approval metadata for this operation.
    pub migration: Migration,
    /// Effective target schema selected during lowering.
    pub schema: String,
    /// Bare target table name.
    pub table: String,
    /// Bare target column name.
    pub column: String,
    /// The dialect-rendered target type, exactly as the renderer spells it.
    pub ddl_type: String,
}

/// One explicit import-time identity-generator reconciliation.
///
/// This remains structured until apply so the backend validates the live
/// identity/sequence association and performs a monotonic comparison while the
/// project migration lock is held. `writes_quiesced` is the operator's named
/// assertion; it is audit/status metadata, not something the engine can prove.
#[derive(Debug, Clone)]
pub struct SynchronizeIdentityStep {
    /// Journal marker and execution metadata for this operation.
    pub migration: Migration,
    /// Effective target schema selected during lowering.
    pub schema: String,
    /// Bare target table name.
    pub table: String,
    /// Bare identity column name.
    pub column: String,
    /// Named maintenance window or invariant asserted by the operator.
    pub writes_quiesced: String,
}

/// The dialect reach of an applied plan, MEASURED from its ops at lowering. A
/// separate facet - **not** folded into the identity checksum, and not a wire field.
///
/// # Derived, never declared
///
/// Nothing authors this. A declared reach would be a second source of truth about a
/// question the op list already answers, and the two can disagree the moment an
/// author edits one without the other. The engine asks every registered backend for
/// its own disposition on each op and intersects the answers, so an artifact cannot
/// claim a reach its ops do not have.
///
/// # Where it is enforced, and where it is NOT
///
/// [`admits`](Self::admits) is consulted at APPLY, whole-plan, before the project
/// lock is taken and before any step executes. That placement is the point: the
/// per-target refusals that already exist - lowering declines a privileged op on a
/// backend without the capability, load declines a `dialect()` expression whose legs
/// miss the target - are both checks a PRE-LOWERED plan never faces, and the engine
/// takes the lowering dialect and the apply backend as independent inputs.
///
/// # Why the pinned arm carries a [`DialectId`]
///
/// `PgOnly` could only ever say "Postgres", so a MySQL-only or DuckDB-only artifact
/// had no way to describe itself; `Only(id)` does, and it does so without this enum
/// growing a variant per backend.
///
/// # The two arms are not the whole lattice
///
/// A reach that is a proper subset of the registered backends with more than one
/// member - a `dialect()` expression carrying two legs of three - has no arm here and
/// is carried as `Portable`, which under-refuses. Closing that takes a third arm over
/// a `DialectSet`; it is stated rather than hidden because a reader must not take
/// `Portable` for "proven portable everywhere".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialectScope {
    /// Every registered backend renders this artifact's ops - or the reach could not
    /// be narrowed to exactly one. See the caveat on the enum.
    Portable,
    /// Pinned to ONE dialect: exactly one registered backend can render these ops.
    /// The privileged catalog-object family, the `raw` escape and a single-leg
    /// `dialect({ ... })` expression are what produce it. Never produced by the
    /// `.sql` path, whose text the engine cannot read either way.
    Only(DialectId),
}

impl DialectScope {
    /// Whether this plan may be applied against `target`.
    #[must_use]
    pub fn admits(&self, target: &DialectId) -> bool {
        match self {
            Self::Portable => true,
            Self::Only(id) => id == target,
        }
    }
}

/// A rename lowered to ONE of two **executable STRATEGIES**, selected at lowering
/// by what the deploy target can express.
///
/// The arms name the strategy, not a vendor, and that is load-bearing rather than
/// stylistic: the spellings `PgExpandContract` / `SqliteRebuild` asserted a
/// one-vendor guarantee the lowering does not enforce. The differ's only gate here
/// is `is_sqlite`, so a MySQL rename fell through to the expand-contract author and
/// was wrapped in a variant named for PostgreSQL. That is a MISSING PLAN-TIME
/// REFUSAL, not MySQL support - `docs/dialects.md`, MySQL's registered validation
/// policy, and `lower_ir_rename` all declare MySQL column rename unsupported, and the
/// declarative differ is the lone dissenter. The strategy names are honest about
/// what each arm IS without re-encoding a dialect claim the type cannot keep.
/// `dialect_matrix::plan_vocabulary_names_strategies_not_vendors` holds the line.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum RenameStep {
    /// An ONLINE expand-contract: add the new shape, dual-write, backfill, then
    /// drop the old in a later deploy. PostgreSQL is its legitimate producer.
    ExpandContract(ExpandContractPlan),
    /// An OFFLINE create-copy-swap table rebuild, for a target with no native
    /// `ALTER` for the change. SQLite's 12-step procedure is its one producer.
    TableRebuild(TableRebuild),
}

/// What one step's rollback is known to achieve, as distinct from whether it
/// has reversing SQL at all. See [`PlanStep::reversibility`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepReversibility {
    /// No reversing SQL exists for this step.
    Irreversible,
    /// Reversing SQL exists, and the step destroys data it cannot bring back.
    /// A dropped column is the ordinary case: the column returns, its values
    /// do not.
    StructurallyReversibleLossy,
    /// Reversing SQL exists and nothing establishes what it restores. Raw
    /// `.sql` migrations land here, because their text is opaque to the engine.
    Unassessed,
}

/// One ordered step of the engine's `AppliedPlan`.
#[derive(Debug, Clone)]
pub enum PlanStep {
    /// A transactional or non-txn DDL statement bundle - an existing
    /// [`Migration`] (single `up: String`, no parameter slot).
    Ddl(Migration),
    /// A parameterized DML statement (insert/update/delete) - the net-new
    /// variant.
    Dml {
        /// The journal version this DML step records under (its sub-version).
        /// A `Migration`-less step still needs an identity to journal.
        version: MigrationId,
        /// The authoritative checksum of the complete owning IR artifact. It
        /// includes the typed bind values, so changing data at the same stable
        /// step identity is checksum drift rather than a new execution.
        checksum: Checksum,
        /// Human-readable label for status/diagnostics.
        name: String,
        /// The placeholder SQL. The journal persists the authoritative IR
        /// checksum, which covers this template and its typed binds.
        template: String,
        /// The ordered typed values bound natively to the template.
        binds: Vec<BindValue>,
        /// Structurally known target schema. Backends use this instead of parsing
        /// rendered SQL for safety checks.
        target_schema: String,
        /// Structurally known target table.
        target_table: String,
        /// Authored columns for a structured `onConflict.doUpdate`, when this
        /// statement has one. MySQL carries these to its catalog preflight so it
        /// can prove the target is one complete `UNIQUE`/`PRIMARY` key before
        /// executing its native duplicate-key form. Other DML statements carry
        /// `None`.
        conflict_target: Option<Vec<String>>,
        /// Whether the statement mutates application data. Ordinary insert,
        /// update, and delete steps are true; read-only partition guards are false.
        mutates_data: bool,
        /// `true` => the step's DDL/journal runs inside a transaction.
        transactional: bool,
        /// `true` => data loss (a `delete`); the gate decides.
        destructive: bool,
        /// `true` => explicit operator approval is required even when the
        /// operation is not classified as data loss.
        requires_approval: bool,
        /// The declaring app's `owner_app` - the journal-identity attribution.
        owner_app: String,
    },
    /// A crash-safe batched data backfill.
    Backfill {
        /// Stable journal/progress identity derived from the owning plan and this
        /// step's ordered position, never from the transform content.
        version: MigrationId,
        /// The authoritative checksum of the complete owning IR artifact.
        checksum: Checksum,
        /// The structured, resumable backfill operation.
        spec: BackfillSpec,
    },
    /// A live-catalog-validated primary-key lifecycle mutation.
    AlterPrimaryKey(AlterPrimaryKeyStep),
    /// A column retype the backend restates from the live column definition.
    AlterColumnType(AlterColumnTypeStep),
    /// A live-catalog-validated, monotonic identity-generator reconciliation.
    SynchronizeIdentity(SynchronizeIdentityStep),
    /// A rename, lowered to ONE of two dialect-distinct executable shapes.
    OnlineRename(RenameStep),
}

impl PlanStep {
    /// Whether this step carries data loss for the destructive/approval gate.
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        match self {
            PlanStep::Ddl(m) => m.flags.destructive,
            PlanStep::Dml { destructive, .. } => *destructive,
            PlanStep::Backfill { .. } => true,
            PlanStep::AlterPrimaryKey(step) => step.migration.flags.destructive,
            PlanStep::AlterColumnType(step) => step.migration.flags.destructive,
            PlanStep::SynchronizeIdentity(step) => step.migration.flags.destructive,
            PlanStep::OnlineRename(RenameStep::TableRebuild(rb)) => rb.migration.flags.destructive,
            PlanStep::OnlineRename(RenameStep::ExpandContract(_)) => false,
        }
    }

    /// The version-id the per-version
    /// [`ApprovalScope`](crate::approval::ApprovalScope) gate consults for this step, when the
    /// step is SCOPE-GATED, else `None`.
    #[must_use]
    pub fn approval_scope_version(&self) -> Option<&str> {
        match self {
            PlanStep::Ddl(m) if m.flags.destructive || m.flags.requires_approval => {
                Some(m.version.as_str())
            }
            PlanStep::Dml {
                version,
                destructive,
                requires_approval,
                ..
            } if *destructive || *requires_approval => Some(version.as_str()),
            PlanStep::Backfill { version, .. } => Some(version.as_str()),
            PlanStep::AlterPrimaryKey(step)
                if step.migration.flags.destructive || step.migration.flags.requires_approval =>
            {
                Some(step.migration.version.as_str())
            }
            PlanStep::AlterColumnType(step)
                if step.migration.flags.destructive || step.migration.flags.requires_approval =>
            {
                Some(step.migration.version.as_str())
            }
            PlanStep::SynchronizeIdentity(step)
                if step.migration.flags.destructive || step.migration.flags.requires_approval =>
            {
                Some(step.migration.version.as_str())
            }
            PlanStep::OnlineRename(RenameStep::TableRebuild(rb))
                if rb.migration.flags.destructive || rb.migration.flags.requires_approval =>
            {
                Some(rb.migration.version.as_str())
            }
            PlanStep::OnlineRename(RenameStep::ExpandContract(ec)) => {
                Some(ec.group_version().as_str())
            }
            _ => None,
        }
    }

    /// The OPTIONAL backend capability this step needs, and the version that names
    /// it, or `None` when the step needs nothing beyond applying a migration.
    ///
    /// The SINGLE source of truth for what a plan REQUIRES, so the plan-wide
    /// capability preflight and the seam that would otherwise discover the gap
    /// mid-apply cannot disagree about which steps are at issue. It answers by
    /// STRATEGY, never by vendor: an [`ExpandContract`](RenameStep::ExpandContract)
    /// needs an online path because that is what an expand-contract IS, not because
    /// PostgreSQL is its usual producer.
    #[must_use]
    pub fn required_capability(&self) -> Option<(BackendCapability, &str)> {
        match self {
            PlanStep::OnlineRename(RenameStep::ExpandContract(ec)) => Some((
                BackendCapability::OnlineSchemaChange,
                ec.group_version().as_str(),
            )),
            PlanStep::Ddl(_)
            | PlanStep::Dml { .. }
            | PlanStep::Backfill { .. }
            | PlanStep::AlterPrimaryKey(_)
            | PlanStep::AlterColumnType(_)
            | PlanStep::SynchronizeIdentity(_)
            | PlanStep::OnlineRename(RenameStep::TableRebuild(_)) => None,
        }
    }

    /// What rolling this step back can be relied on to do.
    ///
    /// [`has_down`](Self::has_down) answers only whether reversing SQL exists.
    /// This separates the case where it exists and destroys data from the case
    /// where nothing establishes what it restores.
    ///
    /// There is deliberately no "restores the prior state" answer. Proving that
    /// needs positive evidence per operation, and a step carries rendered SQL
    /// rather than the ops it came from, so the only signal left is the
    /// `destructive` flag - whose `false` on a raw `.sql` migration means nobody
    /// declared one, not that the engine established anything. Treating an
    /// absent declaration as proof is the confusion this method exists to end.
    #[must_use]
    pub fn reversibility(&self) -> StepReversibility {
        if !self.has_down() {
            return StepReversibility::Irreversible;
        }
        if self.is_destructive() {
            return StepReversibility::StructurallyReversibleLossy;
        }
        StepReversibility::Unassessed
    }

    /// Whether this step has a defined `down` for plan-level rollback.
    #[must_use]
    pub fn has_down(&self) -> bool {
        match self {
            PlanStep::Ddl(m) => m.down.is_some(),
            PlanStep::Dml { .. }
            | PlanStep::Backfill { .. }
            | PlanStep::AlterPrimaryKey(_)
            | PlanStep::AlterColumnType(_)
            | PlanStep::SynchronizeIdentity(_)
            | PlanStep::OnlineRename(_) => false,
        }
    }

    /// The table this step STRUCTURALLY targets, when known (interlock
    /// touched-set).
    #[must_use]
    pub fn touched_table(&self) -> Option<&str> {
        match self {
            PlanStep::OnlineRename(RenameStep::ExpandContract(ec)) => match &ec.intent {
                OnlineIntent::RenameColumn { table, .. } => Some(table.as_str()),
            },
            PlanStep::OnlineRename(RenameStep::TableRebuild(rb)) => Some(rb.spec.table.as_str()),
            PlanStep::Backfill { spec, .. } => Some(spec.table.as_str()),
            PlanStep::AlterPrimaryKey(step) => Some(step.table.as_str()),
            PlanStep::AlterColumnType(step) => Some(step.table.as_str()),
            PlanStep::SynchronizeIdentity(step) => Some(step.table.as_str()),
            PlanStep::Dml { target_table, .. } => Some(target_table.as_str()),
            PlanStep::Ddl(_) => None,
        }
    }
}

/// The set of tables a plan's steps STRUCTURALLY touch (interlock).
#[must_use]
pub fn tables_touched_by(steps: &[PlanStep]) -> std::collections::BTreeSet<String> {
    steps
        .iter()
        .filter_map(|s| s.touched_table().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod touched_table_tests {
    use super::*;

    #[test]
    fn backfill_step_contributes_its_table() {
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "members".into(),
            cursor_columns: vec!["id".into()],
            cursor_stability: zero_migrate_ir::ir::CursorStability::GuardUpdates,
            cursor_contract: None,
            batch_size: 100,
            set_clause: "x = 1".into(),
            per_row: Default::default(),
            filter: None,
            name: "bf".into(),
        };
        let step = PlanStep::Backfill {
            version: MigrationId::derive("test_backfill", b"members"),
            checksum: Checksum::of(&zero_migrate_ir::migration::ChecksumInput {
                up: "backfill members",
                down: None,
                flags: &zero_migrate_ir::migration::MigrationFlags::default(),
                owner_app: "app",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            }),
            spec,
        };
        assert_eq!(step.touched_table(), Some("members"));
        assert!(tables_touched_by(std::slice::from_ref(&step)).contains("members"));
    }
}
