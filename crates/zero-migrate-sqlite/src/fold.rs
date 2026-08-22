use std::collections::BTreeMap;

use zero_migrate_backend::error::IrLowerError;
use zero_migrate_backend::fold::CatalogFoldPolicy;
use zero_migrate_backend::snapshot::{
    ColumnSnapshot, PartitionSnapshot, SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use zero_migrate_backend::stored_ddl::StoredDdl;
use zero_migrate_ir::ir::ColType;

#[derive(Debug)]
pub(crate) struct SqliteCatalogFoldPolicy;

pub(crate) static POLICY: SqliteCatalogFoldPolicy = SqliteCatalogFoldPolicy;

impl CatalogFoldPolicy for SqliteCatalogFoldPolicy {
    fn allocate_implicit_relation_name(
        &self,
        default_name: &str,
        _tables: &BTreeMap<String, TableSnapshot>,
        _partitions: &BTreeMap<String, PartitionSnapshot>,
        _views: &BTreeMap<String, ViewSnapshot>,
        _sequences: &BTreeMap<String, SequenceSnapshot>,
    ) -> String {
        default_name.to_string()
    }

    fn rowid_storage_generates(&self, stored_create_sql: Option<&str>, data_type: &str) -> bool {
        if stored_create_sql.is_some() {
            data_type.trim().eq_ignore_ascii_case("INTEGER")
        } else {
            matches!(
                data_type.trim().to_ascii_lowercase().as_str(),
                "integer" | "int" | "bigint" | "smallint" | "boolean"
            )
        }
    }

    fn stored_table_allows_rowid(&self, stored_create_sql: Option<&str>) -> bool {
        stored_create_sql
            .is_none_or(|stored| !crate::stored_ddl::PARSER.create_is_without_rowid(stored))
    }

    fn stored_primary_key_allows_rowid(
        &self,
        stored_create_sql: Option<&str>,
        column: &str,
    ) -> bool {
        stored_create_sql.is_none_or(|stored| {
            !crate::stored_ddl::PARSER.create_is_without_rowid(stored)
                && !crate::stored_ddl::PARSER.inline_primary_key_is_desc(stored, column)
        })
    }

    fn primary_key_keeps_identity(&self, target_columns: Option<&[String]>, column: &str) -> bool {
        target_columns.is_some_and(|target| target == [column])
    }

    fn reusable_primary_index(
        &self,
        _snapshot: &TableSnapshot,
        _columns: &[String],
        _current_primary_key_name: Option<&str>,
    ) -> Option<String> {
        None
    }

    fn rename_primary_key_after_table_rename(&self, _snapshot: &mut TableSnapshot, _to: &str) {
        // SQLite retains the authored primary-key name across a table rename.
    }

    fn is_native_uuid_type(&self, _data_type: &str) -> bool {
        false
    }

    fn materialized_named_type_metadata(
        &self,
        _ty: &ColType,
        _default_schema: &str,
    ) -> Result<Option<(String, String)>, IrLowerError> {
        Ok(None)
    }

    fn inline_enum_check(
        &self,
        column: &str,
        values: &[String],
    ) -> Result<Option<String>, IrLowerError> {
        let col = zero_migrate_backend::dml::quote_ident_for_backend(
            "column",
            column,
            &crate::dml::RENDERER,
        )
        .map_err(IrLowerError::DmlAssemble)?;
        Ok(Some(format!(
            "CHECK ({col} IN ({}))",
            render_enum_values(values)
        )))
    }

    fn inline_enum_type(&self, _values: &[String]) -> Option<String> {
        None
    }

    fn folds_check_constraint_identity(&self) -> bool {
        // SQLite retains CHECK text in stored CREATE DDL, but the current fold
        // scope does not reconcile arbitrary authored CHECK identity.
        false
    }

    fn physical_type_inputs_equal(&self, _left: &ColumnSnapshot, _right: &ColumnSnapshot) -> bool {
        // SQLite's finalizer is an explicit no-op, so every answer is safe.
        false
    }
}

fn render_enum_values(values: &[String]) -> String {
    values
        .iter()
        .map(|v| zero_migrate_backend::dml::sql_string_literal(v))
        .collect::<Vec<_>>()
        .join(", ")
}
