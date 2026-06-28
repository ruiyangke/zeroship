use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::transport::RowSet;
use crate::apply::drift::DriftError;
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot,
    SchemaSnapshot, TableSnapshot, ViewSnapshot,
};

#[derive(Debug, Clone, Default)]
pub struct MysqlCatalogRowSets {
    pub tables: RowSet,
    pub columns: RowSet,
    pub statistics: RowSet,
    pub table_constraints: RowSet,
    pub key_column_usage: RowSet,
    pub views: RowSet,
}

pub fn rowsets_to_schema_snapshot(
    rows: MysqlCatalogRowSets,
) -> Result<SchemaSnapshot, DriftError> {
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    let mut views: BTreeMap<String, ViewSnapshot> = BTreeMap::new();

    for row in &rows.tables.rows {
        let name = required_str(row, &["TABLE_NAME", "table_name"])?;
        tables.insert(
            name.to_string(),
            TableSnapshot {
                columns: Vec::new(),
                indexes: Vec::new(),
                constraints: Vec::new(),
                runtime_options: Default::default(),
                comment: optional_nonempty_str(row, &["TABLE_COMMENT", "table_comment"]),
                stored_create_sql: None,
            },
        );
    }

    for row in &rows.columns.rows {
        let table = required_str(row, &["TABLE_NAME", "table_name"])?;
        let Some(snapshot) = tables.get_mut(table) else {
            continue;
        };
        let nullable = optional_str(row, &["IS_NULLABLE", "is_nullable"])
            .is_some_and(|v| v.eq_ignore_ascii_case("YES"));
        let column_type = optional_str(row, &["COLUMN_TYPE", "column_type"])
            .or_else(|| optional_str(row, &["DATA_TYPE", "data_type"]))
            .unwrap_or("text");
        snapshot.columns.push(ColumnSnapshot {
            name: required_str(row, &["COLUMN_NAME", "column_name"])?.to_string(),
            data_type: mysql_canonical_type(column_type),
            nullable,
            comment: optional_nonempty_str(row, &["COLUMN_COMMENT", "column_comment"]),
            ..Default::default()
        });
    }

    for ((table, index), group) in group_statistics(rows.statistics.rows) {
        let Some(snapshot) = tables.get_mut(&table) else {
            continue;
        };
        let first = group.first().expect("group_statistics never emits empty groups");
        let unique = numeric_field(first, &["NON_UNIQUE", "non_unique"]).unwrap_or(1) == 0;
        let access_method = optional_str(first, &["INDEX_TYPE", "index_type"])
            .unwrap_or("BTREE")
            .to_ascii_lowercase();
        let comment = optional_nonempty_str(first, &["INDEX_COMMENT", "index_comment"]);
        let mut columns = Vec::new();
        for row in group {
            if let Some(column) = optional_nonempty_str(&row, &["COLUMN_NAME", "column_name"]) {
                columns.push(column);
            }
        }
        snapshot.indexes.push(IndexSnapshot {
            name: index,
            unique,
            columns: columns.clone(),
            elements: columns.into_iter().map(IndexElementSnapshot::column).collect(),
            access_method,
            predicate: None,
            opclass: None,
            comment,
        });
    }

    let key_columns = group_key_columns(rows.key_column_usage.rows);
    for row in &rows.table_constraints.rows {
        let table = required_str(row, &["TABLE_NAME", "table_name"])?;
        let name = required_str(row, &["CONSTRAINT_NAME", "constraint_name"])?;
        let Some(snapshot) = tables.get_mut(table) else {
            continue;
        };
        let kind = required_str(row, &["CONSTRAINT_TYPE", "constraint_type"])?
            .to_ascii_uppercase();
        let cols = key_columns
            .get(&(table.to_string(), name.to_string()))
            .cloned()
            .unwrap_or_default();
        snapshot.constraints.push(ConstraintSnapshot {
            name: name.to_string(),
            kind: kind.clone(),
            definition: mysql_constraint_definition(&kind, &cols),
            comment: None,
        });
    }

    for row in &rows.views.rows {
        let name = required_str(row, &["TABLE_NAME", "table_name", "VIEW_NAME", "view_name"])?;
        views.insert(
            name.to_string(),
            ViewSnapshot {
                materialized: false,
                columns: None,
                definition: optional_nonempty_str(
                    row,
                    &["VIEW_DEFINITION", "view_definition"],
                ),
                comment: None,
            },
        );
    }

    for table in tables.values_mut() {
        table.columns.sort_by(|a, b| a.name.cmp(&b.name));
        table.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        table.constraints.sort_by(|a, b| a.name.cmp(&b.name));
    }

    Ok(SchemaSnapshot {
        tables,
        views,
        named_types: BTreeMap::new(),
        sequences: BTreeMap::new(),
        roles: BTreeMap::new(),
        schemas: BTreeMap::new(),
        extensions: BTreeMap::new(),
    })
}

pub(crate) fn mysql_canonical_type(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.starts_with("enum(") {
        return lower;
    }
    match lower.as_str() {
        "tinyint(1)" | "bool" | "boolean" => "boolean".to_string(),
        "json" => "json".to_string(),
        "datetime" | "datetime(6)" | "timestamp" | "timestamp(6)" => {
            "timestamp with time zone".to_string()
        }
        "date" => "date".to_string(),
        "double" | "float" => "double precision".to_string(),
        "decimal" | "decimal(65,30)" | "decimal(65, 30)" => "numeric".to_string(),
        "blob" | "longblob" | "binary" | "varbinary" => "bytea".to_string(),
        "longtext" | "mediumtext" | "text" | "varchar(191)" => "text".to_string(),
        "int" | "integer" => "integer".to_string(),
        "bigint" => "bigint".to_string(),
        _ => strip_integer_display_width(&lower).unwrap_or(lower),
    }
}

fn strip_integer_display_width(lower: &str) -> Option<String> {
    let (prefix, canonical) = [
        ("tinyint", "tinyint"),
        ("smallint", "smallint"),
        ("mediumint", "integer"),
        ("int", "integer"),
        ("integer", "integer"),
        ("bigint", "bigint"),
    ]
    .into_iter()
    .find(|(prefix, _)| lower.starts_with(&format!("{prefix}(")))?;
    let rest = lower.strip_prefix(prefix)?.trim_start();
    let width = rest.strip_prefix('(')?.strip_suffix(')')?;
    if width.chars().all(|c| c.is_ascii_digit()) {
        Some(canonical.to_string())
    } else {
        None
    }
}

fn group_statistics(rows: Vec<Map<String, Value>>) -> BTreeMap<(String, String), Vec<Map<String, Value>>> {
    let mut keyed: Vec<(String, String, i64, Map<String, Value>)> = rows
        .into_iter()
        .filter_map(|row| {
            let table = optional_str(&row, &["TABLE_NAME", "table_name"])?.to_string();
            let index = optional_str(&row, &["INDEX_NAME", "index_name"])?.to_string();
            let seq = numeric_field(&row, &["SEQ_IN_INDEX", "seq_in_index"]).unwrap_or(0);
            Some((table, index, seq, row))
        })
        .collect();
    keyed.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));

    let mut out: BTreeMap<(String, String), Vec<Map<String, Value>>> = BTreeMap::new();
    for (table, index, _seq, row) in keyed {
        out.entry((table, index)).or_default().push(row);
    }
    out
}

fn group_key_columns(rows: Vec<Map<String, Value>>) -> BTreeMap<(String, String), Vec<String>> {
    let mut keyed: Vec<(String, String, i64, String)> = rows
        .into_iter()
        .filter_map(|row| {
            let table = optional_str(&row, &["TABLE_NAME", "table_name"])?.to_string();
            let constraint = optional_str(&row, &["CONSTRAINT_NAME", "constraint_name"])?
                .to_string();
            let ordinal = numeric_field(&row, &["ORDINAL_POSITION", "ordinal_position"])
                .unwrap_or(0);
            let column = optional_str(&row, &["COLUMN_NAME", "column_name"])?.to_string();
            Some((table, constraint, ordinal, column))
        })
        .collect();
    keyed.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));

    let mut out: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (table, constraint, _ordinal, column) in keyed {
        out.entry((table, constraint)).or_default().push(column);
    }
    out
}

fn mysql_constraint_definition(kind: &str, columns: &[String]) -> String {
    let list = columns
        .iter()
        .map(|c| format!("`{}`", c.replace('`', "``")))
        .collect::<Vec<_>>()
        .join(", ");
    match kind {
        "PRIMARY KEY" => format!("PRIMARY KEY ({list})"),
        "UNIQUE" => format!("UNIQUE ({list})"),
        "FOREIGN KEY" => format!("FOREIGN KEY ({list})"),
        _ => String::new(),
    }
}

fn required_str<'a>(row: &'a Map<String, Value>, keys: &[&str]) -> Result<&'a str, DriftError> {
    optional_str(row, keys).ok_or_else(|| {
        DriftError::Snapshot(format!(
            "MySQL catalog row missing required field {}",
            keys.join("/")
        ))
    })
}

fn optional_nonempty_str(row: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    optional_str(row, keys)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn optional_str<'a>(row: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| match row.get(*key) {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    })
}

fn numeric_field(row: &Map<String, Value>, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| match row.get(*key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn row(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("row must be object"),
        }
    }

    #[test]
    fn mysql_canonical_type_folds_information_schema_spellings() {
        assert_eq!(mysql_canonical_type("tinyint(1)"), "boolean");
        assert_eq!(mysql_canonical_type("int(11)"), "integer");
        assert_eq!(mysql_canonical_type("BIGINT(20)"), "bigint");
        assert_eq!(mysql_canonical_type("JSON"), "json");
        assert_eq!(mysql_canonical_type("datetime(6)"), "timestamp with time zone");
        assert_eq!(mysql_canonical_type("varchar(191)"), "text");
    }

    #[test]
    fn rowsets_to_schema_snapshot_maps_mysql_information_schema_rows() {
        let snapshot = rowsets_to_schema_snapshot(MysqlCatalogRowSets {
            tables: RowSet {
                rows: vec![row(json!({
                    "TABLE_NAME": "users",
                    "TABLE_COMMENT": "app users"
                }))],
            },
            columns: RowSet {
                rows: vec![
                    row(json!({
                        "TABLE_NAME": "users",
                        "COLUMN_NAME": "active",
                        "COLUMN_TYPE": "tinyint(1)",
                        "IS_NULLABLE": "NO"
                    })),
                    row(json!({
                        "TABLE_NAME": "users",
                        "COLUMN_NAME": "created_at",
                        "COLUMN_TYPE": "datetime(6)",
                        "IS_NULLABLE": "NO"
                    })),
                    row(json!({
                        "TABLE_NAME": "users",
                        "COLUMN_NAME": "profile",
                        "COLUMN_TYPE": "json",
                        "IS_NULLABLE": "YES"
                    })),
                    row(json!({
                        "TABLE_NAME": "users",
                        "COLUMN_NAME": "visits",
                        "COLUMN_TYPE": "int(11)",
                        "IS_NULLABLE": "NO"
                    })),
                ],
            },
            statistics: RowSet {
                rows: vec![row(json!({
                    "TABLE_NAME": "users",
                    "INDEX_NAME": "idx_users_active",
                    "NON_UNIQUE": 1,
                    "SEQ_IN_INDEX": 1,
                    "COLUMN_NAME": "active",
                    "INDEX_TYPE": "BTREE"
                }))],
            },
            table_constraints: RowSet {
                rows: vec![row(json!({
                    "TABLE_NAME": "users",
                    "CONSTRAINT_NAME": "PRIMARY",
                    "CONSTRAINT_TYPE": "PRIMARY KEY"
                }))],
            },
            key_column_usage: RowSet {
                rows: vec![row(json!({
                    "TABLE_NAME": "users",
                    "CONSTRAINT_NAME": "PRIMARY",
                    "COLUMN_NAME": "id",
                    "ORDINAL_POSITION": 1
                }))],
            },
            views: RowSet::default(),
        })
        .expect("maps");

        let users = snapshot.tables.get("users").expect("users table");
        assert_eq!(users.comment.as_deref(), Some("app users"));
        let types = users
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                ("active", "boolean", false),
                ("created_at", "timestamp with time zone", false),
                ("profile", "json", true),
                ("visits", "integer", false),
            ]
        );
        assert_eq!(users.indexes.len(), 1);
        assert_eq!(users.indexes[0].name, "idx_users_active");
        assert_eq!(users.indexes[0].access_method, "btree");
        assert_eq!(users.constraints[0].definition, "PRIMARY KEY (`id`)");
    }
}
