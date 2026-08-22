//! Backend-owned catalog-fold semantics.
//!
//! The structural fold is shared, but several of the facts it mirrors are not:
//! implicit relation-name allocation, rowid aliases, primary-key adoption,
//! catalog CHECK scope, named enum/domain representation, and the physical type
//! metadata a backend can safely re-derive.  This required contract keeps those
//! answers in the registering backend without teaching core a closed vendor list.

use std::collections::BTreeMap;

use crate::error::IrLowerError;
use crate::snapshot::{
    ColumnSnapshot, PartitionSnapshot, SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use zero_migrate_ir::expr::Expr;
use zero_migrate_ir::ir::{ColType, ValueFormat};
use zero_migrate_ir::precondition::PreconditionCheck;

/// Exact character storage used when a backend requires both sides of a
/// reference to agree on more than a portable text/case-sensitivity token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceTextStorage {
    pub character_set: String,
    pub collation: String,
}

/// The backend-owned scalar family for one resumable-backfill cursor column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldCursorScalarType {
    Int64,
    Decimal,
    String,
}

/// The backend-owned comparison contract for one cursor column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldCursorComparison {
    Default,
    CaseInsensitive,
    NamedCollation {
        schema: Option<String>,
        name: String,
    },
    ExactText {
        character_set: String,
        collation: String,
    },
}

/// The physical cursor facts a backend derives from its own catalog spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldCursorColumnContract {
    pub scalar_type: FoldCursorScalarType,
    pub database_type: String,
    pub comparison: FoldCursorComparison,
}

/// A live-database feature required by one vendor's lowering of typed IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldDatabaseFeature {
    UuidV4Generation,
    UuidV7Generation,
    UuidValidation,
    TypeIdValidation,
    UlidValidation,
}

/// Relative strength of the backend-owned catalog evidence carried by a table
/// snapshot.
///
/// The ordering is intentional. A malformed or hand-built snapshot may carry
/// more than one backend's evidence at once; selecting the strongest evidence
/// preserves the historical `stored definition` -> `exact text storage` ->
/// `type override` precedence without teaching core which backend owns any of
/// those carriers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SnapshotProvenanceStrength {
    TypeOverride,
    ExactTextStorage,
    StoredTableDefinition,
}

/// A catalog-fold refusal whose operator-facing bytes are owned by the selected
/// backend.
///
/// These are structural operation shapes, not vendor identities. Core decides
/// when a shape is unsupported; the backend that registered the policy states
/// the exact refusal it wants an operator to receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogFoldRefusal {
    AlterPrimaryKeyRowidGeneration,
    AddColumnIdentity,
    CreateTableCheckConstraint,
    CreateTableUniqueConstraint,
    CreateTableExclusionConstraint,
    CreateTableNonBtreeIndex,
    AddCheckConstraint,
    AddExclusionConstraint,
}

/// Type metadata that cannot survive the descriptor bridge's deliberately small
/// token vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorTypeOverride {
    pub data_type: String,
    pub ddl_type: Option<String>,
    pub quote_literal_default_as_text: bool,
}

/// Vendor policy consumed by the neutral catalog fold.
///
/// Every method is required. A fourth backend must state each catalog rule in
/// its own crate; it cannot inherit one shipping backend's behavior by omission.
pub trait CatalogFoldPolicy: std::fmt::Debug + Sync {
    /// Identify this backend's catalog evidence in a table snapshot.
    ///
    /// `None` means the snapshot carries no evidence attributable to this
    /// backend. Every backend writes its own required answer; core only compares
    /// the returned evidence strengths.
    fn snapshot_provenance_strength(
        &self,
        table: &TableSnapshot,
    ) -> Option<SnapshotProvenanceStrength>;

    /// Allocate a backend-canonical implicit relation name.
    fn allocate_implicit_relation_name(
        &self,
        default_name: &str,
        tables: &BTreeMap<String, TableSnapshot>,
        partitions: &BTreeMap<String, PartitionSnapshot>,
        views: &BTreeMap<String, ViewSnapshot>,
        sequences: &BTreeMap<String, SequenceSnapshot>,
    ) -> String;

    /// Whether this declared storage type generates through a rowid alias.
    fn rowid_storage_generates(&self, stored_create_sql: Option<&str>, data_type: &str) -> bool;

    /// Whether retained CREATE DDL permits the table to use rowid aliases at all.
    fn stored_table_allows_rowid(&self, stored_create_sql: Option<&str>) -> bool;

    /// Whether retained CREATE DDL permits `column` to be a rowid alias.
    fn stored_primary_key_allows_rowid(
        &self,
        stored_create_sql: Option<&str>,
        column: &str,
    ) -> bool;

    /// Whether the target primary key preserves a generated column's identity
    /// contract.
    fn primary_key_keeps_identity(&self, target_columns: Option<&[String]>, column: &str) -> bool;

    /// A pre-existing unique index this backend can adopt as the primary key.
    fn reusable_primary_index(
        &self,
        snapshot: &TableSnapshot,
        columns: &[String],
        current_primary_key_name: Option<&str>,
    ) -> Option<String>;

    /// Apply catalog-visible primary-key renaming caused by a table rename.
    fn rename_primary_key_after_table_rename(&self, snapshot: &mut TableSnapshot, to: &str);

    /// Whether this catalog type is the backend's native UUID representation.
    fn is_native_uuid_type(&self, data_type: &str) -> bool;

    /// Resolve a materialized named enum/domain's catalog and DDL spellings.
    fn materialized_named_type_metadata(
        &self,
        ty: &ColType,
        default_schema: &str,
    ) -> Result<Option<(String, String)>, IrLowerError>;

    /// Canonicalize a type spelling for rename compatibility without
    /// discarding backend-significant modifiers.
    fn canonical_rename_type_spelling(&self, ty: &str) -> String;

    /// Render an inline membership CHECK for a non-materialized enum.
    fn inline_enum_check(
        &self,
        column: &str,
        values: &[String],
    ) -> Result<Option<String>, IrLowerError>;

    /// Render a non-materialized enum as an inline native type.
    fn inline_enum_type(&self, values: &[String]) -> Option<String>;

    /// Whether this engine's folded snapshot scope models arbitrary authored
    /// CHECK constraint identity for this backend.
    ///
    /// This is deliberately not phrased as an engine limitation. A backend may
    /// expose CHECK names and clauses while the engine's chosen snapshot scope
    /// still excludes standalone authored CHECK reconciliation.
    fn folds_check_constraint_identity(&self) -> bool;

    /// The exact operator-facing refusal for an unsupported catalog-fold shape.
    fn refusal_message(&self, refusal: CatalogFoldRefusal) -> &'static str;

    /// Whether two column snapshots feed this backend's physical-type finalizer
    /// identical inputs, so catalog-only metadata on the base must be preserved.
    fn physical_type_inputs_equal(&self, left: &ColumnSnapshot, right: &ColumnSnapshot) -> bool;

    /// Select the physical type carrier used to compare a referencing column.
    fn reference_catalog_type<'a>(&self, column: &'a ColumnSnapshot) -> &'a str;

    /// Canonicalize a reference type without discarding backend-significant
    /// integer-width evidence.
    fn canonical_reference_catalog_type(
        &self,
        data_type: &str,
        integer_width_is_logically_proven: bool,
    ) -> String;

    /// Recover explicit character storage from an authored DDL type, when this
    /// backend requires it for foreign-key compatibility.
    fn explicit_reference_text_storage(&self, ddl_type: &str) -> Option<ReferenceTextStorage>;

    /// Recover exact character storage from a live catalog column.
    fn catalog_reference_text_storage(
        &self,
        column: &ColumnSnapshot,
    ) -> Option<ReferenceTextStorage>;

    /// Whether named catalog collation is compared separately from exact text
    /// storage for this backend.
    fn compares_reference_named_collation(&self) -> bool;

    /// Derive the persisted cursor contract for one catalog column.
    fn cursor_column_contract(
        &self,
        column: &ColumnSnapshot,
    ) -> Result<FoldCursorColumnContract, String>;

    /// Apply this backend's grammar around an already-rendered DEFAULT expression.
    fn wrap_default_expr(&self, expr: &Expr, rendered: String) -> String;

    /// Project a column declaration into this backend's live-server requirements.
    fn database_requirement_for_column(
        &self,
        ty: &ColType,
        is_reference: bool,
    ) -> Option<FoldDatabaseFeature>;

    /// Project a logical value-format declaration into live-server requirements.
    fn database_requirement_for_value_format(
        &self,
        value_format: &ValueFormat,
    ) -> Option<FoldDatabaseFeature>;

    /// Project one expression node into live-server requirements. The neutral
    /// walker remains responsible for recursively visiting child expressions.
    fn database_requirement_for_expr(&self, expr: &Expr) -> Option<FoldDatabaseFeature>;

    /// A backend-owned dependency precondition for dropping one column.
    fn drop_column_precondition(&self, table: &str, column: &str) -> Option<PreconditionCheck>;

    /// Whether a type change must remain structured until apply can restate the
    /// server's complete live column definition.
    fn restates_column_type_at_apply(&self) -> bool;

    /// A backend-owned dependency precondition for changing one column's type.
    fn column_type_change_precondition(
        &self,
        table: &str,
        column: &str,
    ) -> Option<PreconditionCheck>;

    /// Refuse an alter-column shape this engine does not yet render for this backend.
    fn alter_column_refusal(&self, op: &'static str) -> Result<(), IrLowerError>;

    /// Spell the populated-default guard used while collapsing a partition.
    fn partition_collapse_mirror_guard(
        &self,
        table_sql: &str,
        key_sql: &str,
        predicate: &str,
    ) -> Result<String, IrLowerError>;

    /// Whether this backend admits expression elements in an index snapshot.
    fn supports_expression_index(&self) -> bool;

    /// Project authored IR types that the descriptor token bridge cannot express.
    fn author_type_override(&self, ty: &ColType) -> Option<AuthorTypeOverride>;
}
