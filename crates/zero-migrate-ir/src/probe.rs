//! Pure data carried by executor-side existence probes.
//!
//! The renderer builds these descriptors while lowering guarded IR, but the
//! migration model stores them. Keeping the data in `model` prevents the migration
//! wire type from depending on render code.

use crate::ir::ExistenceGuard;

/// One declared column's verifiable shape for a `createTable ifNotExists`
/// [`GuardProbe::Table`] probe. Built from the SAME shared snapshot the CREATE
/// renders from, so the `data_type`/`nullable` strings are byte-comparable against
/// introspection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExpectColumn {
    /// Column name.
    pub name: String,
    /// The introspectable data-type spelling (PG type / `SQLite` affinity).
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
            ExistenceGuard::IfNotExists => Self::IfNotExists,
            ExistenceGuard::IfExists => Self::IfExists,
        }
    }
}

/// Keep a defaulted `false` off the serialized wire so an unset marker leaves the
/// probe's stored bytes byte-identical to what earlier versions wrote.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(flag: &bool) -> bool {
    !*flag
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
        /// Decide on index-name OWNERSHIP alone: fail closed when a DIFFERENT
        /// table already owns the name, run the statement otherwise. Carried by an
        /// UNGUARDED `createIndex` on a dialect that scopes index names schema-wide,
        /// where the emitted `IF NOT EXISTS` would otherwise let the engine skip a
        /// create the author never guarded and the journal record it green.
        ///
        /// Does NOT verify the index shape and can never resolve to a satisfied
        /// no-op, so the same-table re-run stays an `IF NOT EXISTS` no-op that
        /// non-transactional crash recovery depends on. Does NOT cover a name a
        /// PostgreSQL identifier truncation would collide into, and does NOT cover
        /// a guarded create (which keeps the full shape verify).
        #[serde(default, skip_serializing_if = "is_false")]
        ownership_only: bool,
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

impl GuardProbe {
    /// The effective schema this probe reads — the `snapshot_schema` argument the
    /// executor passes so the catalog read targets the op's schema.
    #[must_use]
    pub fn schema(&self) -> &str {
        match self {
            Self::Table { schema, .. }
            | Self::Column { schema, .. }
            | Self::Index { schema, .. }
            | Self::Constraint { schema, .. }
            | Self::View { schema, .. }
            | Self::Sequence { schema, .. }
            | Self::NamedType { schema, .. }
            | Self::ColumnPresence { schema, .. } => schema,
        }
    }
}
