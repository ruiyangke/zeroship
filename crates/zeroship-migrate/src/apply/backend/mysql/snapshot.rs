use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::transport::RowSet;
use crate::apply::drift::DriftError;
use crate::model::ir::IndexSortOrder;
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot,
    SchemaSnapshot, TableSnapshot, ViewSnapshot,
};
use crate::render::declarative::{constraintdef_cols, quote_ident_if_needed};

#[derive(Debug, Clone, Default)]
pub struct MysqlCatalogRowSets {
    pub tables: RowSet,
    pub columns: RowSet,
    pub statistics: RowSet,
    pub table_constraints: RowSet,
    pub key_column_usage: RowSet,
    pub referential_constraints: RowSet,
    pub views: RowSet,
}

pub fn rowsets_to_schema_snapshot(
    project_schema: &str,
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

    let key_columns = group_key_columns(rows.key_column_usage.rows)?;
    let referential_constraints =
        group_referential_constraints(&rows.referential_constraints.rows)?;

    for ((table, index), group) in group_statistics(rows.statistics.rows) {
        let Some(snapshot) = tables.get_mut(&table) else {
            continue;
        };
        let name = if index == "PRIMARY" {
            format!("{table}_pkey")
        } else {
            index
        };
        let first = group.first().expect("group_statistics never emits empty groups");
        let unique = numeric_field(first, &["NON_UNIQUE", "non_unique"]).unwrap_or(1) == 0;
        let access_method = optional_str(first, &["INDEX_TYPE", "index_type"])
            .unwrap_or("BTREE")
            .to_ascii_lowercase();
        let comment = optional_nonempty_str(first, &["INDEX_COMMENT", "index_comment"]);
        let mut columns = Vec::new();
        let mut elements = Vec::new();
        for row in group {
            if let Some(column) = optional_nonempty_str(&row, &["COLUMN_NAME", "column_name"]) {
                let element = match optional_str(&row, &["COLLATION", "collation"]) {
                    Some(collation) if collation.eq_ignore_ascii_case("D") => {
                        IndexElementSnapshot::column_ordered(column.clone(), IndexSortOrder::Desc)
                    }
                    _ => IndexElementSnapshot::column(column.clone()),
                };
                columns.push(column);
                elements.push(element);
            }
        }
        if mysql_fk_backing_index(&table, &name, &columns, &key_columns, &referential_constraints) {
            continue;
        }
        snapshot.indexes.push(IndexSnapshot {
            name,
            unique,
            columns: columns.clone(),
            elements,
            access_method,
            predicate: None,
            opclass: None,
            comment,
        });
    }

    for row in &rows.table_constraints.rows {
        let table = required_str(row, &["TABLE_NAME", "table_name"])?;
        let raw_name = required_str(row, &["CONSTRAINT_NAME", "constraint_name"])?;
        let Some(snapshot) = tables.get_mut(table) else {
            continue;
        };
        let kind = required_str(row, &["CONSTRAINT_TYPE", "constraint_type"])?
            .to_ascii_uppercase();
        let name = if kind == "PRIMARY KEY" && raw_name == "PRIMARY" {
            format!("{table}_pkey")
        } else {
            raw_name.to_string()
        };
        let key_group = key_columns
            .get(&(table.to_string(), raw_name.to_string()))
            .cloned()
            .unwrap_or_default();
        let cols = key_group.local_columns.clone();
        let definition = if kind == "FOREIGN KEY" {
            let Some(referential) =
                referential_constraints.get(&(table.to_string(), raw_name.to_string()))
            else {
                return Err(DriftError::Snapshot(format!(
                    "MySQL FOREIGN KEY {table}.{raw_name} missing referential constraint row"
                )));
            };
            mysql_foreign_key_definition(project_schema, table, raw_name, &key_group, referential)?
        } else {
            mysql_constraint_definition(&kind, &cols)
        };
        snapshot.constraints.push(ConstraintSnapshot {
            name,
            kind: kind.clone(),
            definition,
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

fn mysql_fk_backing_index(
    table: &str,
    index: &str,
    columns: &[String],
    key_columns: &BTreeMap<(String, String), KeyColumnGroup>,
    referential_constraints: &BTreeMap<(String, String), ReferentialConstraint>,
) -> bool {
    let key = (table.to_string(), index.to_string());
    referential_constraints.contains_key(&key)
        && key_columns
            .get(&key)
            .is_some_and(|group| group.local_columns == columns)
}

pub(crate) fn mysql_canonical_type(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.starts_with("enum(") {
        return lower;
    }
    match lower.as_str() {
        "tinyint(1)" | "bool" | "boolean" => "boolean".to_string(),
        "json" => "jsonb".to_string(),
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

#[derive(Debug, Clone, Default)]
struct KeyColumnGroup {
    local_columns: Vec<String>,
    referenced_table: Option<String>,
    referenced_columns: Vec<String>,
}

#[derive(Debug, Clone)]
struct ReferentialConstraint {
    referenced_table: String,
    on_update: &'static str,
    on_delete: &'static str,
}

fn group_key_columns(
    rows: Vec<Map<String, Value>>,
) -> Result<BTreeMap<(String, String), KeyColumnGroup>, DriftError> {
    let mut keyed: Vec<(String, String, i64, String, Option<String>, Option<String>)> = rows
        .into_iter()
        .filter_map(|row| {
            let table = optional_str(&row, &["TABLE_NAME", "table_name"])?.to_string();
            let constraint = optional_str(&row, &["CONSTRAINT_NAME", "constraint_name"])?
                .to_string();
            let ordinal = numeric_field(&row, &["ORDINAL_POSITION", "ordinal_position"])
                .unwrap_or(0);
            let column = optional_str(&row, &["COLUMN_NAME", "column_name"])?.to_string();
            let referenced_table =
                optional_str(&row, &["REFERENCED_TABLE_NAME", "referenced_table_name"])
                    .map(ToString::to_string);
            let referenced_column =
                optional_str(&row, &["REFERENCED_COLUMN_NAME", "referenced_column_name"])
                    .map(ToString::to_string);
            Some((table, constraint, ordinal, column, referenced_table, referenced_column))
        })
        .collect();
    keyed.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));

    let mut out: BTreeMap<(String, String), KeyColumnGroup> = BTreeMap::new();
    for (table, constraint, _ordinal, column, referenced_table, referenced_column) in keyed {
        let group = out.entry((table.clone(), constraint.clone())).or_default();
        group.local_columns.push(column);
        if let Some(referenced_table) = referenced_table {
            match &group.referenced_table {
                Some(existing) if existing != &referenced_table => {
                    return Err(DriftError::Snapshot(format!(
                        "MySQL FOREIGN KEY {table}.{constraint} references multiple tables: \
                         {existing} and {referenced_table}"
                    )));
                }
                Some(_) => {}
                None => group.referenced_table = Some(referenced_table),
            }
        }
        if let Some(referenced_column) = referenced_column {
            group.referenced_columns.push(referenced_column);
        }
    }
    Ok(out)
}

fn group_referential_constraints(
    rows: &[Map<String, Value>],
) -> Result<BTreeMap<(String, String), ReferentialConstraint>, DriftError> {
    let mut out = BTreeMap::new();
    for row in rows {
        let table = required_str(row, &["TABLE_NAME", "table_name"])?;
        let constraint = required_str(row, &["CONSTRAINT_NAME", "constraint_name"])?;
        let referenced_table = required_str(
            row,
            &["REFERENCED_TABLE_NAME", "referenced_table_name"],
        )?
        .to_string();
        let on_update = mysql_referential_action(
            required_str(row, &["UPDATE_RULE", "update_rule"])?,
        )?;
        let on_delete = mysql_referential_action(
            required_str(row, &["DELETE_RULE", "delete_rule"])?,
        )?;
        let key = (table.to_string(), constraint.to_string());
        if out.insert(
            key.clone(),
            ReferentialConstraint { referenced_table, on_update, on_delete },
        )
        .is_some()
        {
            return Err(DriftError::Snapshot(format!(
                "MySQL duplicate referential constraint row for {}.{}",
                key.0, key.1
            )));
        }
    }
    Ok(out)
}

fn mysql_referential_action(rule: &str) -> Result<&'static str, DriftError> {
    match rule.trim().to_ascii_uppercase().as_str() {
        "CASCADE" => Ok("CASCADE"),
        // InnoDB checks both immediately; canonicalize the default action to the
        // spelling this snapshot renderer omits from FK definitions.
        "RESTRICT" | "NO ACTION" => Ok("NO ACTION"),
        "SET NULL" => Ok("SET NULL"),
        "SET DEFAULT" => Ok("SET DEFAULT"),
        other => Err(DriftError::Snapshot(format!(
            "MySQL catalog returned unknown referential action {other:?}"
        ))),
    }
}

fn mysql_foreign_key_definition(
    project_schema: &str,
    table: &str,
    constraint: &str,
    key_group: &KeyColumnGroup,
    referential: &ReferentialConstraint,
) -> Result<String, DriftError> {
    if key_group.local_columns.is_empty() {
        return Err(DriftError::Snapshot(format!(
            "MySQL FOREIGN KEY {table}.{constraint} has no local columns"
        )));
    }
    if key_group.referenced_columns.is_empty() {
        return Err(DriftError::Snapshot(format!(
            "MySQL FOREIGN KEY {table}.{constraint} has no referenced columns"
        )));
    }
    if let Some(kcu_target) = &key_group.referenced_table {
        if kcu_target != &referential.referenced_table {
            return Err(DriftError::Snapshot(format!(
                "MySQL FOREIGN KEY {table}.{constraint} target mismatch: \
                 KEY_COLUMN_USAGE={kcu_target}, REFERENTIAL_CONSTRAINTS={}",
                referential.referenced_table
            )));
        }
    }

    let mut definition = format!(
        "FOREIGN KEY ({}) REFERENCES {}.{}({})",
        constraintdef_cols(&key_group.local_columns),
        quote_ident_if_needed(project_schema),
        quote_ident_if_needed(&referential.referenced_table),
        constraintdef_cols(&key_group.referenced_columns),
    );
    if referential.on_update != "NO ACTION" {
        definition.push_str(" ON UPDATE ");
        definition.push_str(referential.on_update);
    }
    if referential.on_delete != "NO ACTION" {
        definition.push_str(" ON DELETE ");
        definition.push_str(referential.on_delete);
    }
    Ok(definition)
}

fn mysql_constraint_definition(kind: &str, columns: &[String]) -> String {
    let list = columns
        .iter()
        .map(String::as_str)
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
        assert_eq!(mysql_canonical_type("JSON"), "jsonb");
        assert_eq!(mysql_canonical_type("datetime(6)"), "timestamp with time zone");
        assert_eq!(mysql_canonical_type("varchar(191)"), "text");
    }

    #[test]
    fn rowsets_to_schema_snapshot_maps_mysql_information_schema_rows() {
        let snapshot = rowsets_to_schema_snapshot(
            "app",
            MysqlCatalogRowSets {
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
                referential_constraints: RowSet::default(),
                views: RowSet::default(),
            },
        )
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
                ("profile", "jsonb", true),
                ("visits", "integer", false),
            ]
        );
        assert_eq!(users.indexes.len(), 1);
        assert_eq!(users.indexes[0].name, "idx_users_active");
        assert_eq!(users.indexes[0].access_method, "btree");
        assert_eq!(users.constraints[0].name, "users_pkey");
        assert_eq!(users.constraints[0].definition, "PRIMARY KEY (id)");
    }

    #[test]
    fn rowsets_to_schema_snapshot_preserves_mysql_fk_target_and_actions() {
        let snapshot = rowsets_to_schema_snapshot(
            "app",
            MysqlCatalogRowSets {
                tables: RowSet {
                    rows: vec![
                        row(json!({ "TABLE_NAME": "parents" })),
                        row(json!({ "TABLE_NAME": "children" })),
                    ],
                },
                columns: RowSet::default(),
                statistics: RowSet {
                    rows: vec![row(json!({
                        "TABLE_NAME": "children",
                        "INDEX_NAME": "children_parent_fkey",
                        "NON_UNIQUE": 1,
                        "SEQ_IN_INDEX": 1,
                        "COLUMN_NAME": "parent_id",
                        "INDEX_TYPE": "BTREE"
                    }))],
                },
                table_constraints: RowSet {
                    rows: vec![row(json!({
                        "TABLE_NAME": "children",
                        "CONSTRAINT_NAME": "children_parent_fkey",
                        "CONSTRAINT_TYPE": "FOREIGN KEY"
                    }))],
                },
                key_column_usage: RowSet {
                    rows: vec![row(json!({
                        "TABLE_NAME": "children",
                        "CONSTRAINT_NAME": "children_parent_fkey",
                        "COLUMN_NAME": "parent_id",
                        "ORDINAL_POSITION": 1,
                        "REFERENCED_TABLE_NAME": "parents",
                        "REFERENCED_COLUMN_NAME": "id"
                    }))],
                },
                referential_constraints: RowSet {
                    rows: vec![row(json!({
                        "TABLE_NAME": "children",
                        "CONSTRAINT_NAME": "children_parent_fkey",
                        "REFERENCED_TABLE_NAME": "parents",
                        "UPDATE_RULE": "CASCADE",
                        "DELETE_RULE": "SET NULL"
                    }))],
                },
                views: RowSet::default(),
            },
        )
        .expect("maps FK");

        let children = snapshot.tables.get("children").expect("children table");
        assert!(
            children.indexes.is_empty(),
            "MySQL implicit FK backing index must not drift as an authored index: {:?}",
            children.indexes
        );
        let fk = children
            .constraints
            .iter()
            .find(|c| c.name == "children_parent_fkey")
            .expect("FK constraint");
        assert_eq!(fk.kind, "FOREIGN KEY");
        assert_eq!(
            fk.definition,
            "FOREIGN KEY (parent_id) REFERENCES app.parents(id) ON UPDATE CASCADE \
             ON DELETE SET NULL"
        );
    }

    #[test]
    fn rowsets_to_schema_snapshot_omits_mysql_restrict_no_action_defaults() {
        let snapshot = rowsets_to_schema_snapshot(
            "app",
            MysqlCatalogRowSets {
                tables: RowSet {
                    rows: vec![
                        row(json!({ "TABLE_NAME": "parents" })),
                        row(json!({ "TABLE_NAME": "children" })),
                    ],
                },
                columns: RowSet::default(),
                statistics: RowSet {
                    rows: vec![row(json!({
                        "TABLE_NAME": "children",
                        "INDEX_NAME": "children_parent_fkey",
                        "NON_UNIQUE": 1,
                        "SEQ_IN_INDEX": 1,
                        "COLUMN_NAME": "parent_id",
                        "INDEX_TYPE": "BTREE"
                    }))],
                },
                table_constraints: RowSet {
                    rows: vec![row(json!({
                        "TABLE_NAME": "children",
                        "CONSTRAINT_NAME": "children_parent_fkey",
                        "CONSTRAINT_TYPE": "FOREIGN KEY"
                    }))],
                },
                key_column_usage: RowSet {
                    rows: vec![row(json!({
                        "TABLE_NAME": "children",
                        "CONSTRAINT_NAME": "children_parent_fkey",
                        "COLUMN_NAME": "parent_id",
                        "ORDINAL_POSITION": 1,
                        "REFERENCED_TABLE_NAME": "parents",
                        "REFERENCED_COLUMN_NAME": "id"
                    }))],
                },
                referential_constraints: RowSet {
                    rows: vec![row(json!({
                        "TABLE_NAME": "children",
                        "CONSTRAINT_NAME": "children_parent_fkey",
                        "REFERENCED_TABLE_NAME": "parents",
                        "UPDATE_RULE": "RESTRICT",
                        "DELETE_RULE": "NO ACTION"
                    }))],
                },
                views: RowSet::default(),
            },
        )
        .expect("maps FK");

        let children = snapshot.tables.get("children").expect("children table");
        let fk = children
            .constraints
            .iter()
            .find(|c| c.name == "children_parent_fkey")
            .expect("FK constraint");
        assert_eq!(
            fk.definition,
            "FOREIGN KEY (parent_id) REFERENCES app.parents(id)"
        );
    }
}
