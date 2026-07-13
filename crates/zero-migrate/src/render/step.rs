//! Low-level lowered-plan step values.

use crate::model::migration::{Migration, MigrationId};
use crate::render::declarative::SqliteRebuild;
use crate::render::expand_contract::{ExpandContractPlan, OnlineIntent};
use crate::model::backfill::BackfillSpec;

/// The dialect reach of an applied plan, derived from its ops. A separate,
/// journaled facet — **not** folded into the identity checksum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialectScope {
    /// Applies faithfully to both Postgres and SQLite.
    Both,
    /// Postgres-only (a `PgOnly` `op.raw` artifact); refused against a SQLite
    /// deploy target at load. Never produced by the `.sql` path.
    PgOnly,
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
#[derive(Debug, Clone, PartialEq)]
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
        /// Human-readable label for status/diagnostics.
        name: String,
        /// The placeholder SQL (the journal hashes this; the binds fold into the
        /// plan checksum).
        template: String,
        /// The ordered typed values bound natively to the template.
        binds: Vec<BindValue>,
        /// `true` ⇒ the step's DDL/journal runs inside a transaction.
        transactional: bool,
        /// `true` ⇒ data loss (a `delete`); the gate decides.
        destructive: bool,
        /// The declaring app's `owner_app` — the journal-identity attribution.
        owner_app: String,
    },
    /// A crash-safe batched data backfill.
    Backfill(BackfillSpec),
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
            PlanStep::Backfill(_) => false,
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
            PlanStep::Ddl(m) if m.flags.destructive => Some(m.version.as_str()),
            PlanStep::Dml { version, destructive, .. } if *destructive => Some(version.as_str()),
            PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb))
                if rb.migration.flags.destructive =>
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

    /// Whether this step has a defined `down` for plan-level rollback.
    #[must_use]
    pub fn has_down(&self) -> bool {
        match self {
            PlanStep::Ddl(m) => m.down.is_some(),
            PlanStep::Dml { .. } | PlanStep::Backfill(_) | PlanStep::OnlineRename(_) => false,
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
            PlanStep::Backfill(spec) => Some(spec.table.as_str()),
            PlanStep::Ddl(_) | PlanStep::Dml { .. } => None,
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
            cursor_column: "id".into(),
            batch_size: 100,
            set_clause: "x = 1".into(),
            filter: None,
            name: "bf".into(),
        };
        let step = PlanStep::Backfill(spec);
        assert_eq!(step.touched_table(), Some("members"));
        assert!(tables_touched_by(std::slice::from_ref(&step)).contains("members"));
    }
}
