//! Pure data carried by executor-side existence probes.
//!
//! The renderer builds these descriptors while lowering guarded IR, but the
//! migration model stores them. Keeping the data in `model` prevents the migration
//! wire type from depending on render code.

use crate::model::ir::ExistenceGuard;

/// One declared column's verifiable shape for a `createTable ifNotExists`
/// [`GuardProbe::Table`] probe. Built from the SAME shared snapshot the CREATE
/// renders from, so the `data_type`/`nullable` strings are byte-comparable against
/// introspection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExpectColumn {
    /// Column name.
    pub name: String,
    /// The introspectable data-type spelling (PG type / SQLite affinity).
    pub data_type: String,
    /// Declared nullability.
    pub nullable: bool,
}

/// The guard DIRECTION carried on a probe (a 1:1 copy of [`ExistenceGuard`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GuardDir {
    /// Run only if the target object is ABSENT (`create*`/`add*`).
    IfNotExists,
    /// Run only if the target object is PRESENT (`drop*`/`rename`/`alter*`).
    IfExists,
}

impl From<ExistenceGuard> for GuardDir {
    fn from(g: ExistenceGuard) -> Self {
        match g {
            ExistenceGuard::IfNotExists => GuardDir::IfNotExists,
            ExistenceGuard::IfExists => GuardDir::IfExists,
        }
    }
}

/// A render-time-resolved, dialect-neutral descriptor of WHAT to probe and WHICH
/// shape to verify. Built in `lower_one_op` from the op and stamped onto each
/// lowered migration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GuardProbe {
    /// `createTable ifNotExists` or `dropTable ifExists`.
    Table {
        /// The effective schema the table lives in.
        schema: String,
        /// The table name.
        table: String,
        /// Guard direction.
        direction: GuardDir,
        /// The declared columns to shape-verify (`ifNotExists`); empty for the
        /// presence-only `ifExists` drop.
        expect_columns: Vec<ExpectColumn>,
    },
    /// `addColumn ifNotExists` or `dropColumn ifExists`.
    Column {
        /// The effective schema.
        schema: String,
        /// The table the column belongs to.
        table: String,
        /// The column name.
        column: String,
        /// Guard direction.
        direction: GuardDir,
        /// The declared `(data_type, nullable)` to verify (`ifNotExists`); `None`
        /// for the presence-only `ifExists` drop.
        expect: Option<(String, bool)>,
    },
    /// `createIndex ifNotExists` or `dropIndex ifExists`.
    Index {
        /// The effective schema.
        schema: String,
        /// The table the index covers.
        table: String,
        /// The index name.
        name: String,
        /// Guard direction.
        direction: GuardDir,
        /// The declared `(unique, columns)` to verify (`ifNotExists`); `None` for
        /// the presence-only `ifExists` drop.
        expect: Option<(bool, Vec<String>)>,
    },
    /// `addConstraint ifNotExists` or `dropConstraint ifExists`.
    Constraint {
        /// The effective schema.
        schema: String,
        /// The table the constraint belongs to.
        table: String,
        /// The constraint name.
        name: String,
        /// Guard direction.
        direction: GuardDir,
        /// The declared catalog kind to compare (`ifNotExists`); `None` for the
        /// presence-only `ifExists` drop.
        expect_kind: Option<String>,
        /// The declared constraint definition in exact `pg_get_constraintdef`
        /// spelling when the authoring path can produce a byte-comparable body.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_definition: Option<String>,
    },
    /// `dropView ifExists`: presence-only on the top-level view name.
    View {
        /// The effective schema.
        schema: String,
        /// The view name.
        name: String,
        /// Guard direction.
        direction: GuardDir,
    },
    /// `createSequence ifNotExists` or `dropSequence ifExists`.
    Sequence {
        /// The effective schema.
        schema: String,
        /// The sequence name.
        name: String,
        /// Guard direction.
        direction: GuardDir,
    },
    /// `createEnum` / `createDomain` / drops.
    NamedType {
        /// The effective schema.
        schema: String,
        /// The named type.
        name: String,
        /// Stable kind token (`"enum"` / `"domain"`).
        kind: String,
        /// Guard direction.
        direction: GuardDir,
    },
    /// Presence guard for a named column where no physical shape comparison is
    /// needed.
    ColumnPresence {
        /// The effective schema.
        schema: String,
        /// The table the column belongs to.
        table: String,
        /// The column name.
        column: String,
        /// Guard direction.
        direction: GuardDir,
    },
}
