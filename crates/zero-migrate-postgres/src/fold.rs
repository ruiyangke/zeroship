use std::collections::BTreeMap;

use zero_migrate_backend::error::IrLowerError;
use zero_migrate_backend::fold::CatalogFoldPolicy;
use zero_migrate_backend::snapshot::{
    ColumnSnapshot, IndexElementSnapshot, PartitionSnapshot, SequenceSnapshot, TableSnapshot,
    ViewSnapshot,
};
use zero_migrate_ir::ir::ColType;

#[derive(Debug)]
pub(crate) struct PostgresCatalogFoldPolicy;

pub(crate) static POLICY: PostgresCatalogFoldPolicy = PostgresCatalogFoldPolicy;

impl CatalogFoldPolicy for PostgresCatalogFoldPolicy {
    fn allocate_implicit_relation_name(
        &self,
        default_name: &str,
        tables: &BTreeMap<String, TableSnapshot>,
        partitions: &BTreeMap<String, PartitionSnapshot>,
        views: &BTreeMap<String, ViewSnapshot>,
        sequences: &BTreeMap<String, SequenceSnapshot>,
    ) -> String {
        let name_is_taken = |candidate: &str| {
            tables.contains_key(candidate)
                || partitions.contains_key(candidate)
                || views.contains_key(candidate)
                || sequences.contains_key(candidate)
                || tables
                    .values()
                    .any(|snapshot| snapshot.indexes.iter().any(|index| index.name == candidate))
        };
        let relation_count = tables.len()
            + partitions.len()
            + views.len()
            + sequences.len()
            + tables
                .values()
                .map(|snapshot| snapshot.indexes.len())
                .sum::<usize>();
        (0..=relation_count)
            .map(|suffix| {
                if suffix == 0 {
                    default_name.to_string()
                } else {
                    format!("{default_name}{suffix}")
                }
            })
            .find(|candidate| !name_is_taken(candidate))
            .expect(
                "one more implicit relation-name candidate than relations must leave a free name",
            )
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
        target_columns.is_some_and(|target| target.iter().any(|candidate| candidate == column))
    }

    fn reusable_primary_index(
        &self,
        snap: &TableSnapshot,
        columns: &[String],
        current_primary_key_name: Option<&str>,
    ) -> Option<String> {
        snap.indexes
            .iter()
            .find(|index| {
                let constraint_owned = snap.constraints.iter().any(|constraint| {
                    constraint.name == index.name
                        && matches!(
                            constraint.kind.as_str(),
                            "PRIMARY KEY" | "UNIQUE" | "EXCLUDE"
                        )
                });
                index.name != current_primary_key_name.unwrap_or_default()
                    && !constraint_owned
                    && index.unique
                    && index.columns == columns
                    && index.access_method == "btree"
                    && index.predicate.is_none()
                    && index.include.is_empty()
                    && !index.only
                    && index.elements.len() == columns.len()
                    && index.elements.iter().all(|element| {
                        matches!(
                            element,
                            IndexElementSnapshot::Column {
                                order: None | Some(zero_migrate_ir::ir::IndexSortOrder::Asc),
                                opclass: None,
                                collation: None,
                                ..
                            }
                        )
                    })
            })
            .map(|index| index.name.clone())
    }

    fn rename_primary_key_after_table_rename(&self, _snapshot: &mut TableSnapshot, _to: &str) {
        // PostgreSQL stores a primary-key constraint name independently of its table.
    }

    fn is_native_uuid_type(&self, data_type: &str) -> bool {
        data_type.eq_ignore_ascii_case("uuid")
    }

    fn materialized_named_type_metadata(
        &self,
        ty: &ColType,
        default_schema: &str,
    ) -> Result<Option<(String, String)>, IrLowerError> {
        let (name, schema) = match ty {
            ColType::Enum { name, schema } | ColType::Domain { name, schema } => {
                (name, schema.as_deref().unwrap_or(default_schema))
            }
            _ => return Ok(None),
        };
        Ok(Some((
            pg_type_data_type(schema, name),
            pg_type_qname(schema, name)?,
        )))
    }

    fn inline_enum_check(
        &self,
        _column: &str,
        _values: &[String],
    ) -> Result<Option<String>, IrLowerError> {
        Ok(None)
    }

    fn inline_enum_type(&self, _values: &[String]) -> Option<String> {
        None
    }

    fn folds_check_constraint_identity(&self) -> bool {
        true
    }

    fn physical_type_inputs_equal(&self, _left: &ColumnSnapshot, _right: &ColumnSnapshot) -> bool {
        // PostgreSQL's finalizer is an explicit no-op, so every answer is safe.
        false
    }
}

fn quote_engine_ident(what: &'static str, ident: &str) -> Result<String, IrLowerError> {
    zero_migrate_backend::dml::quote_ident_for_backend(what, ident, &crate::dml::RENDERER)
        .map_err(IrLowerError::DmlAssemble)
}

fn pg_type_qname(schema: &str, name: &str) -> Result<String, IrLowerError> {
    Ok(format!(
        "{}.{}",
        quote_engine_ident("schema", schema)?,
        quote_engine_ident("type", name)?
    ))
}

fn pg_type_data_type(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}
