//! Low-level lowered-plan step values.

use crate::model::backfill::BackfillSpec;
use crate::model::migration::{Checksum, Migration, MigrationId};
use crate::render::declarative::SqliteRebuild;
use crate::render::expand_contract::{ExpandContractPlan, OnlineIntent};

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

    /// Whether this step has a defined `down` for plan-level rollback.
    #[must_use]
    pub fn has_down(&self) -> bool {
        match self {
            PlanStep::Ddl(m) => m.down.is_some(),
            PlanStep::Dml { .. } | PlanStep::Backfill { .. } | PlanStep::OnlineRename(_) => false,
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
