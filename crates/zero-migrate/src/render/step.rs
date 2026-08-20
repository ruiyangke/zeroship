//! Low-level lowered-plan step values.

use zero_migrate_ir::dialect::DialectId;

use crate::model::backfill::BackfillSpec;
use crate::model::ir::AlterPrimaryKeyAction;
use crate::model::migration::{Checksum, Migration, MigrationId};
use crate::render::declarative::SqliteRebuild;
use crate::render::expand_contract::{ExpandContractPlan, OnlineIntent};

/// The dialect reach of an applied plan, derived from its ops. A separate,
/// journaled facet — **not** folded into the identity checksum.
///
/// The pinned arm carries a [`DialectId`] rather than naming a vendor in its own
/// variant. `PgOnly` could only ever say "Postgres", so a MySQL-only or
/// DuckDB-only artifact had no way to describe itself; `Only(id)` does, and it
/// does so without this enum growing a variant per backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialectScope {
    /// Applies faithfully to every dialect the artifact's ops support.
    Portable,
    /// Pinned to ONE dialect (a vendor `op.raw` artifact); refused against any
    /// other deploy target at load. Never produced by the `.sql` path.
    Only(DialectId),
}

impl DialectScope {
    /// The reach an artifact pinned to PostgreSQL has. The spelling `PgOnly`
    /// used to be a variant; it is now the one value that variant could hold.
    #[must_use]
    pub const fn pg_only() -> Self {
        Self::Only(zero_migrate_ir::dialect::POSTGRES)
    }

    /// Whether this plan may be applied against `target`.
    #[must_use]
    pub fn admits(self, target: DialectId) -> bool {
        match self {
            Self::Portable => true,
            Self::Only(id) => id == target,
        }
    }
}

/// A rename lowered to ONE of two **dialect-distinct executable shapes**, chosen
/// by the deploy-target dialect at lowering.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum RenameStep {
    /// **Postgres**: an online expand-contract.
    PgExpandContract(ExpandContractPlan),
    /// **SQLite**: an OFFLINE 12-step table rebuild.
    SqliteRebuild(SqliteRebuild),
}

/// A typed scalar bound into a parameterized [`PlanStep::Dml`] statement.
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
/// MySQL has no `ALTER COLUMN … TYPE`. It has `MODIFY COLUMN`, which takes the
/// COMPLETE column definition and silently DISCARDS every facet the statement
/// leaves out — measured in `tests/mysql_engine/mysql_setcolumntype_restate.rs`,
/// where a bare `MODIFY COLUMN label varchar(128)` destroys the column's
/// `NOT NULL`, its `DEFAULT`, its `COLLATE` and its `COMMENT` in one statement,
/// with no warning.
///
/// So the statement cannot be written until the current definition is known, and
/// the current definition is not in the op: `Op::SetColumnType` carries one field.
/// This step stays STRUCTURED until apply for exactly the reason
/// [`AlterPrimaryKeyStep`] does — the backend reads `SHOW CREATE TABLE` under the
/// migration lock and restates the clause the server itself reports, so the answer
/// cannot go stale between the read and the `ALTER`.
///
/// WHY NOT AT LOWER TIME, which would need no new step at all: the apply path DOES
/// have the live column there ([`crate::LiveSchema::table_snapshots`] is populated
/// from a real catalog read by `engine.rs` before it lowers). But
/// `ColumnSnapshot` is a LOSSY projection of a MySQL column — the same test
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

/// One ordered step of an [`AppliedPlan`](crate::render::plan::AppliedPlan).
#[derive(Debug, Clone)]
pub enum PlanStep {
    /// A transactional or non-txn DDL statement bundle — an existing
    /// [`Migration`] (single `up: String`, no parameter slot).
    Ddl(Migration),
    /// A parameterized DML statement (insert/update/delete) — the net-new
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
        /// `true` ⇒ the step's DDL/journal runs inside a transaction.
        transactional: bool,
        /// `true` ⇒ data loss (a `delete`); the gate decides.
        destructive: bool,
        /// `true` ⇒ explicit operator approval is required even when the
        /// operation is not classified as data loss.
        requires_approval: bool,
        /// The declaring app's `owner_app` — the journal-identity attribution.
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
            PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => rb.migration.flags.destructive,
            PlanStep::OnlineRename(RenameStep::PgExpandContract(_)) => false,
        }
    }

    /// The version-id the per-version
    /// [`ApprovalScope`](crate::ApprovalScope) gate consults for this step, when the
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
            PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb))
                if rb.migration.flags.destructive || rb.migration.flags.requires_approval =>
            {
                Some(rb.migration.version.as_str())
            }
            PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => Some(
                ec.expand
                    .first()
                    .map_or_else(|| ec.trigger_version.as_str(), |e1| e1.version.as_str()),
            ),
            _ => None,
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
            PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => match &ec.intent {
                OnlineIntent::RenameColumn { table, .. } => Some(table.as_str()),
            },
            PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => Some(rb.spec.table.as_str()),
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
            cursor_stability: crate::model::ir::CursorStability::GuardUpdates,
            cursor_contract: None,
            batch_size: 100,
            set_clause: "x = 1".into(),
            per_row: Default::default(),
            filter: None,
            name: "bf".into(),
        };
        let step = PlanStep::Backfill {
            version: MigrationId::derive("test_backfill", b"members"),
            checksum: Checksum::of(&crate::model::migration::ChecksumInput {
                up: "backfill members",
                down: None,
                flags: &crate::model::migration::MigrationFlags::default(),
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
