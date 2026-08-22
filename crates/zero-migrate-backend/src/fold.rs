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
use zero_migrate_ir::ir::ColType;

/// Vendor policy consumed by the neutral catalog fold.
///
/// Every method is required. A fourth backend must state each catalog rule in
/// its own crate; it cannot inherit one shipping backend's behavior by omission.
pub trait CatalogFoldPolicy: std::fmt::Debug + Sync {
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

    /// Whether two column snapshots feed this backend's physical-type finalizer
    /// identical inputs, so catalog-only metadata on the base must be preserved.
    fn physical_type_inputs_equal(&self, left: &ColumnSnapshot, right: &ColumnSnapshot) -> bool;
}
