//! Minimal authoritative MySQL application-schema catalog snapshot.

use std::collections::BTreeMap;

use crate::apply::drift::DriftError;
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;
use crate::model::ir::IndexSortOrder;
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot,
    MysqlTextStorageSnapshot, SchemaSnapshot, TableSnapshot,
};

#[derive(Debug)]
struct IndexParts {
    unique: bool,
    access_method: String,
    columns: Vec<String>,
    elements: Vec<IndexElementSnapshot>,
}

/// Normalize MySQL's catalog collation name into the portable
/// `caseSensitive` intent carried by [`ColumnSnapshot`]. MySQL's built-in
/// character collations encode that property in their final token:
/// `_ci` is case-insensitive, while `_cs` and `_bin` are case-sensitive.
/// Non-character columns report no collation and retain the default `None`.
fn case_sensitive_from_collation(collation: Option<&str>) -> Result<Option<bool>, DriftError> {
    let Some(collation) = collation else {
        return Ok(None);
    };
    let normalized = collation.trim().to_ascii_lowercase();
    let tokens = normalized.split('_').collect::<Vec<_>>();
    if tokens.contains(&"ci") {
        Ok(Some(false))
    } else if tokens.contains(&"cs") || tokens.contains(&"bin") || normalized == "binary" {
        // `None` is the canonical snapshot spelling for the default
        // case-sensitive intent; `Some(true)` is deliberately not emitted.
        Ok(None)
    } else {
        Err(DriftError::Snapshot(format!(
            "MySQL catalog returned an unrecognized column collation {collation:?}"
        )))
    }
}

fn mysql_text_storage(
    character_set: Option<&str>,
    collation: Option<&str>,
) -> Result<Option<MysqlTextStorageSnapshot>, DriftError> {
    match (character_set, collation) {
        (None, None) => Ok(None),
        (Some(character_set), Some(collation)) => {
            let character_set = character_set.trim().to_ascii_lowercase();
            let collation = collation.trim().to_ascii_lowercase();
            if character_set.is_empty() || collation.is_empty() {
                return Err(DriftError::Snapshot(
                    "MySQL catalog returned empty character-set/collation metadata".to_string(),
                ));
            }
            Ok(Some(MysqlTextStorageSnapshot {
                character_set,
                collation,
            }))
        }
        _ => Err(DriftError::Snapshot(
            "MySQL catalog returned incomplete character-set/collation metadata".to_string(),
        )),
    }
}

/// Read base tables, columns, and ordered index keys from
/// `information_schema`. Every query scopes itself to the configured application
/// database with a bind; the migration metadata database is never included.
pub(crate) async fn snapshot_schema<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<SchemaSnapshot, DriftError> {
    let schema = cfg.project_schema.as_str();
    let table_rows = conn
        .query(
            "SELECT TABLE_NAME AS table_name
               FROM information_schema.TABLES
              WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE'
              ORDER BY TABLE_NAME",
            &[schema.into()],
        )
        .await?;
    let mut tables = BTreeMap::new();
    for row in table_rows {
        let name: String = row.try_get("table_name")?;
        tables.insert(
            name,
            TableSnapshot {
                columns: Vec::new(),
                indexes: Vec::new(),
                constraints: Vec::new(),
                runtime_options: Default::default(),
                partition_by: None,
                comment: None,
                stored_create_sql: None,
            },
        );
    }

    let column_rows = conn
        .query(
            "SELECT TABLE_NAME AS table_name,
                    COLUMN_NAME AS column_name,
                    COLUMN_TYPE AS column_type,
                    CHARACTER_SET_NAME AS character_set_name,
                    COLLATION_NAME AS collation_name,
                    IS_NULLABLE AS is_nullable,
                    ORDINAL_POSITION AS ordinal_position
               FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ?
              ORDER BY TABLE_NAME, ORDINAL_POSITION",
            &[schema.into()],
        )
        .await?;
    for row in column_rows {
        let table_name: String = row.try_get("table_name")?;
        let Some(table) = tables.get_mut(&table_name) else {
            continue;
        };
        let raw_type: String = row.try_get("column_type")?;
        let character_set: Option<String> = row.try_get("character_set_name")?;
        let collation: Option<String> = row.try_get("collation_name")?;
        let mysql_text_storage =
            mysql_text_storage(character_set.as_deref(), collation.as_deref())?;
        let nullable: String = row.try_get("is_nullable")?;
        table.columns.push(ColumnSnapshot {
            name: row.try_get("column_name")?,
            data_type: crate::schema::query::mysql_canonical_type(&raw_type),
            nullable: nullable.eq_ignore_ascii_case("YES"),
            default: None,
            ddl_type_override: None,
            inline_checks: Vec::new(),
            generated: None,
            identity: None,
            case_sensitive: case_sensitive_from_collation(collation.as_deref())?,
            mysql_text_storage,
            encryption_sentinel: None,
            comment_sentinel: None,
            comment: None,
        });
    }

    let index_rows = conn
        .query(
            "SELECT TABLE_NAME AS table_name,
                    INDEX_NAME AS index_name,
                    NON_UNIQUE AS non_unique,
                    SEQ_IN_INDEX AS seq_in_index,
                    COLUMN_NAME AS column_name,
                    SUB_PART AS sub_part,
                    COLLATION AS index_collation,
                    INDEX_TYPE AS index_type,
                    EXPRESSION AS expression
               FROM information_schema.STATISTICS
              WHERE TABLE_SCHEMA = ?
              ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX",
            &[schema.into()],
        )
        .await?;
    let mut indexes = BTreeMap::<(String, String), IndexParts>::new();
    for row in index_rows {
        let table_name: String = row.try_get("table_name")?;
        if !tables.contains_key(&table_name) {
            continue;
        }
        let index_name: String = row.try_get("index_name")?;
        let non_unique: i64 = row.try_get("non_unique")?;
        let sequence: i64 = row.try_get("seq_in_index")?;
        let index_type: String = row.try_get("index_type")?;
        let key = (table_name, index_name);
        let parts = indexes.entry(key.clone()).or_insert_with(|| IndexParts {
            unique: non_unique == 0,
            access_method: index_type.to_ascii_lowercase(),
            columns: Vec::new(),
            elements: Vec::new(),
        });
        if parts.unique != (non_unique == 0)
            || !parts.access_method.eq_ignore_ascii_case(&index_type)
            || sequence != i64::try_from(parts.elements.len() + 1).unwrap_or(i64::MAX)
        {
            return Err(DriftError::Snapshot(format!(
                "MySQL catalog returned inconsistent index metadata for {}.{}",
                key.0, key.1
            )));
        }

        let column: Option<String> = row.try_get("column_name")?;
        let prefix: Option<i64> = row.try_get("sub_part")?;
        let expression: Option<String> = row.try_get("expression")?;
        let index_collation: Option<String> = row.try_get("index_collation")?;
        let descending = index_collation.is_some_and(|value| value.eq_ignore_ascii_case("D"));
        match (column, prefix, expression) {
            (Some(column), None, None) => {
                parts.columns.push(column.clone());
                parts.elements.push(if descending {
                    IndexElementSnapshot::column_ordered(column, IndexSortOrder::Desc)
                } else {
                    IndexElementSnapshot::column(column)
                });
            }
            (Some(column), Some(length), None) => {
                // A prefix key is not proof that the full column set is unique.
                // Keep it as an expression and deliberately omit it from
                // `columns`, so host lowering cannot mistake it for a full key.
                parts.elements.push(IndexElementSnapshot::Expr(format!(
                    "mysql_prefix({column}, {length})"
                )));
            }
            (None, None, Some(expression)) => {
                parts.elements.push(IndexElementSnapshot::Expr(expression));
            }
            _ => {
                return Err(DriftError::Snapshot(format!(
                    "MySQL catalog returned an unsupported index key for {}.{} at position {sequence}",
                    key.0, key.1
                )));
            }
        }
    }

    for ((table_name, catalog_name), parts) in indexes {
        if let Some(table) = tables.get_mut(&table_name) {
            let is_primary = catalog_name.eq_ignore_ascii_case("PRIMARY");
            let name = if is_primary {
                format!("{table_name}_pkey")
            } else {
                catalog_name
            };
            if is_primary {
                let full_plain_columns = parts.columns.len() == parts.elements.len()
                    && parts
                        .elements
                        .iter()
                        .zip(&parts.columns)
                        .all(|(element, column)| {
                            matches!(element, IndexElementSnapshot::Column { name, .. } if name == column)
                        });
                if !parts.unique || parts.columns.is_empty() || !full_plain_columns {
                    return Err(DriftError::Snapshot(format!(
                        "MySQL catalog returned an invalid PRIMARY key for {table_name:?}: expected a nonempty unique key of full plain columns"
                    )));
                }
                table.constraints.push(ConstraintSnapshot {
                    name: name.clone(),
                    kind: "PRIMARY KEY".to_string(),
                    definition: format!(
                        "PRIMARY KEY ({})",
                        crate::render::declarative::constraintdef_cols(&parts.columns)
                    ),
                    comment: None,
                });
            }
            table.indexes.push(IndexSnapshot {
                name,
                unique: is_primary || parts.unique,
                columns: parts.columns,
                elements: parts.elements,
                access_method: parts.access_method,
                predicate: None,
                include: Vec::new(),
                with: None,
                only: false,
                opclass: None,
                nulls_not_distinct: false,
                comment: None,
            });
        }
    }
    for table in tables.values_mut() {
        table
            .columns
            .sort_by(|left, right| left.name.cmp(&right.name));
        table
            .indexes
            .sort_by(|left, right| left.name.cmp(&right.name));
        table
            .constraints
            .sort_by(|left, right| left.name.cmp(&right.name));
    }

    Ok(SchemaSnapshot {
        tables,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::{case_sensitive_from_collation, mysql_text_storage};

    #[test]
    fn mysql_collations_normalize_to_portable_case_sensitive_intent() {
        assert_eq!(
            case_sensitive_from_collation(Some("utf8mb4_0900_ai_ci")).unwrap(),
            Some(false)
        );
        assert_eq!(
            case_sensitive_from_collation(Some("utf8mb4_ja_0900_as_cs_ks")).unwrap(),
            None
        );
        assert_eq!(
            case_sensitive_from_collation(Some("ascii_bin")).unwrap(),
            None
        );
        assert_eq!(case_sensitive_from_collation(None).unwrap(), None);
        assert!(case_sensitive_from_collation(Some("custom_unknown")).is_err());
    }

    #[test]
    fn mysql_catalog_text_storage_requires_an_exact_pair() {
        let storage = mysql_text_storage(Some("ASCII"), Some("ASCII_BIN"))
            .unwrap()
            .expect("character column metadata");
        assert_eq!(storage.character_set, "ascii");
        assert_eq!(storage.collation, "ascii_bin");
        assert!(mysql_text_storage(Some("ascii"), None).is_err());
        assert!(mysql_text_storage(None, Some("ascii_bin")).is_err());
        assert_eq!(mysql_text_storage(None, None).unwrap(), None);
    }
}
