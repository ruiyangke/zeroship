//! Precondition declaration data.

/// A comparison operator for a [`Precondition::RowCount`] assertion.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

impl CmpOp {
    /// Apply the operator to two i64 operands (`lhs <op> rhs`).
    #[must_use]
    pub const fn apply(self, lhs: i64, rhs: i64) -> bool {
        match self {
            Self::Eq => lhs == rhs,
            Self::Ne => lhs != rhs,
            Self::Lt => lhs < rhs,
            Self::Le => lhs <= rhs,
            Self::Gt => lhs > rhs,
            Self::Ge => lhs >= rhs,
        }
    }
}

/// A single precondition assertion evaluated against the live DB before a
/// migration's `up` runs.
///
/// Structured variants are engine-built parameterized catalog queries
/// (injection-safe); [`Precondition::SqlBoolean`] is untrusted SQL run behind
/// the guard + migrator role + single-read-only-SELECT shape gate. See the
/// module docs.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum Precondition {
    /// The project schema contains a table named `table` (a base table or view).
    TableExists {
        /// The bare table name (no schema qualifier).
        table: String,
    },
    /// The project schema does NOT contain a table named `table`.
    TableNotExists {
        /// The bare table name (no schema qualifier).
        table: String,
    },
    /// The project-schema table `table` has a column named `column`.
    ColumnExists {
        /// The bare table name (no schema qualifier).
        table: String,
        /// The bare column name.
        column: String,
    },
    /// The project-schema table `table` does NOT have a column named `column`.
    ColumnNotExists {
        /// The bare table name (no schema qualifier).
        table: String,
        /// The bare column name.
        column: String,
    },
    /// `count(*)` of the project-schema table `table` compares to `value` under
    /// `op` (e.g. `RowCount { table, op: Eq, value: 0 }` = "the table is empty").
    RowCount {
        /// The bare table name (no schema qualifier).
        table: String,
        /// The comparison operator (`count(*) <op> value`).
        op: CmpOp,
        /// The right-hand operand.
        value: i64,
    },
    /// An UNTRUSTED single read-only `SELECT` returning one boolean column. Run
    /// behind the guard + migrator role + shape gate. The escape hatch for
    /// assertions the structured checks cannot express.
    SqlBoolean {
        /// The read-only `SELECT … ` returning exactly one boolean.
        sql: String,
    },
}

/// What to do when a precondition is **unmet** (evaluates false).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum OnUnmet {
    /// Abort the whole apply (fail-closed): return `PreconditionFailed` and
    /// apply NOTHING for THIS migration. The default - an unmet precondition
    /// usually means the world is not as the migration assumes, and silently
    /// skipping could leave the schema inconsistent.
    ///
    /// **Scope of "halt":** this stops the batch going FORWARD - no
    /// later-in-order migration is applied after the failing one. It does NOT
    /// undo migrations already committed earlier in the same batch: each
    /// migration commits independently (per-migration commit), so the migrations
    /// that succeeded before the halt stay applied. Halt is fail-forward-stop,
    /// not a batch-wide rollback.
    ///
    /// An unmet precondition surfaces as
    /// `zero_migrate::apply::executor::ApplyError::PreconditionFailed`. That type
    /// lives in the engine crate, which depends on this one, so it cannot be linked
    /// from here.
    #[default]
    Halt,
    /// Skip THIS migration this run (do not apply it, do not journal it — it
    /// stays pending and is re-evaluated on the next deploy), and continue with
    /// the rest of the batch. The "apply this once the DB reaches shape X"
    /// idempotent-deploy primitive. A skipped migration's dependents do not run
    /// this batch either (a dependent of a not-yet-applied migration is blocked by
    /// the dependency ordering, in the engine crate's `order_pending`).
    ///
    /// **Skip relies on COMPLETE `depends_on`.** The transitive-skip above only
    /// follows DECLARED dependencies: a later migration that actually depends on
    /// the skipped one but does NOT declare it in `depends_on` is NOT held back —
    /// it will still run, against a schema the skipped migration was supposed to
    /// shape first, and will either fail or (worse) succeed against a stale
    /// shape. This makes `Skip` MORE dangerous than [`OnUnmet::Halt`] when deps
    /// are incomplete: `Halt` aborts the batch forward loudly, whereas `Skip`
    /// silently no-ops the gated migration and lets an undeclared dependent
    /// proceed. Authors choosing `Skip` MUST declare every real dependency.
    Skip,
}

/// One precondition + its unmet policy, carried by a
/// [`Migration`](crate::migration::Migration).
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct PreconditionCheck {
    /// The assertion to evaluate against the live DB.
    pub check: Precondition,
    /// What to do if `check` is unmet (default [`OnUnmet::Halt`]).
    #[serde(default)]
    pub on_unmet: OnUnmet,
}

impl PreconditionCheck {
    /// A check with the default ([`OnUnmet::Halt`]) unmet policy.
    #[must_use]
    pub const fn halt(check: Precondition) -> Self {
        Self {
            check,
            on_unmet: OnUnmet::Halt,
        }
    }

    /// A check that SKIPs the migration when unmet.
    #[must_use]
    pub const fn skip(check: Precondition) -> Self {
        Self {
            check,
            on_unmet: OnUnmet::Skip,
        }
    }
}
