//! Minimal authoritative MySQL application-schema catalog snapshot.

use std::collections::BTreeMap;

use crate::apply::drift::DriftError;
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;
use crate::model::ir::IndexSortOrder;
use crate::model::snapshot::{
    ColumnSnapshot, IndexElementSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot,
};

#[derive(Debug)]
struct IndexParts {
    unique: bool,
    access_method: String,
    columns: Vec<String>,
    elements: Vec<IndexElementSnapshot>,
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
            case_sensitive: None,
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
              WHERE TABLE_SCHEMA = ? AND INDEX_NAME <> 'PRIMARY'
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

    for ((table_name, name), parts) in indexes {
        if let Some(table) = tables.get_mut(&table_name) {
            table.indexes.push(IndexSnapshot {
                name,
                unique: parts.unique,
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
    }

    Ok(SchemaSnapshot {
        tables,
        ..Default::default()
    })
}
