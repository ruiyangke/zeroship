use std::collections::BTreeMap;

use zero_migrate_backend::error::IrLowerError;
use zero_migrate_backend::fold::CatalogFoldPolicy;
use zero_migrate_backend::snapshot::{
    ColumnSnapshot, PartitionSnapshot, SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use zero_migrate_ir::ir::ColType;

#[derive(Debug)]
pub(crate) struct MysqlCatalogFoldPolicy;

pub(crate) static POLICY: MysqlCatalogFoldPolicy = MysqlCatalogFoldPolicy;

impl CatalogFoldPolicy for MysqlCatalogFoldPolicy {
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

    fn rowid_storage_generates(&self, _stored_create_sql: Option<&str>, _data_type: &str) -> bool {
        false
    }

    fn stored_table_allows_rowid(&self, _stored_create_sql: Option<&str>) -> bool {
        false
    }

    fn stored_primary_key_allows_rowid(
        &self,
        _stored_create_sql: Option<&str>,
        _column: &str,
    ) -> bool {
        false
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

    fn rename_primary_key_after_table_rename(&self, snap: &mut TableSnapshot, to: &str) {
        let renamed_pk = format!("{to}_pkey");
        // The index is matched by the constraint's OLD name rather than by
        // a second `format!`, so a primary key the author named something
        // else still moves together with its implicit index instead of
        // splitting into a renamed constraint and an orphaned index.
        let previous: Vec<String> = snap
            .constraints
            .iter()
            .filter(|constraint| constraint.kind == "PRIMARY KEY")
            .map(|constraint| constraint.name.clone())
            .collect();
        for constraint in &mut snap.constraints {
            if constraint.kind == "PRIMARY KEY" {
                constraint.name.clone_from(&renamed_pk);
            }
        }
        for index in &mut snap.indexes {
            if previous.contains(&index.name) {
                index.name.clone_from(&renamed_pk);
            }
        }
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
        _column: &str,
        _values: &[String],
    ) -> Result<Option<String>, IrLowerError> {
        Ok(None)
    }

    fn inline_enum_type(&self, values: &[String]) -> Option<String> {
        Some(format!("ENUM({})", render_enum_values(values)))
    }

    fn folds_check_constraint_identity(&self) -> bool {
        // MySQL exposes CHECK names, clauses, and ENFORCED state. The current fold
        // scope nevertheless does not reconcile arbitrary authored CHECK identity.
        false
    }

    fn physical_type_inputs_equal(&self, left: &ColumnSnapshot, right: &ColumnSnapshot) -> bool {
        left.data_type == right.data_type
            && left.ddl_type_override == right.ddl_type_override
            && left.case_sensitive == right.case_sensitive
            && left.unbounded_text == right.unbounded_text
            && left.type_def == right.type_def
            && left.authored_type == right.authored_type
    }
}

fn render_enum_values(values: &[String]) -> String {
    values
        .iter()
        .map(|v| zero_migrate_backend::dml::mysql_grammar_string_literal(v))
        .collect::<Vec<_>>()
        .join(", ")
}
