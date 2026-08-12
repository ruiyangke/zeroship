//! Minimal authoritative MySQL application-schema catalog snapshot.

use std::collections::BTreeMap;

use crate::apply::drift::DriftError;
use crate::driver::SqlSession;
use crate::model::ir::{IdentityCol, IndexSortOrder};
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IdDefaultSnapshot, IndexElementSnapshot, IndexSnapshot,
    MysqlPhysicalType, MysqlTextStorageSnapshot, SchemaSnapshot, TableSnapshot, ViewSnapshot,
};
use crate::render::value_format::{
    catalog_id_default, catalog_text_id_default, catalog_uuid_id_default, recover_format_check,
    RecoveredFormatCheck,
};
use crate::schema::query::SqlDialect;

#[derive(Debug)]
struct IndexParts {
    unique: bool,
    access_method: String,
    columns: Vec<String>,
    elements: Vec<IndexElementSnapshot>,
}

#[derive(Debug)]
struct ForeignKeyParts {
    referenced_schema: String,
    referenced_table: String,
    update_rule: String,
    delete_rule: String,
    columns: Vec<String>,
    referenced_columns: Vec<String>,
}

fn mysql_fk_action(rule: &str) -> Result<&'static str, DriftError> {
    match rule.trim().to_ascii_uppercase().as_str() {
        "CASCADE" => Ok("CASCADE"),
        "SET NULL" => Ok("SET NULL"),
        "SET DEFAULT" => Ok("SET DEFAULT"),
        // InnoDB implements both spellings as the same immediate-reject action.
        "NO ACTION" | "RESTRICT" => Ok("NO ACTION"),
        other => Err(DriftError::Snapshot(format!(
            "MySQL catalog returned an unrecognized foreign-key action {other:?}"
        ))),
    }
}

fn has_auto_increment(extra: &str) -> bool {
    extra
        .split_whitespace()
        .any(|token| token.eq_ignore_ascii_case("auto_increment"))
}

fn has_default_generated(extra: &str) -> bool {
    extra
        .split_whitespace()
        .any(|token| token.eq_ignore_ascii_case("default_generated"))
}

fn recover_mysql_id_default(
    default: Option<&str>,
    expression_default: bool,
    uuid_surface: bool,
) -> IdDefaultSnapshot {
    if uuid_surface {
        catalog_uuid_id_default(default, SqlDialect::Mysql, Some(expression_default))
    } else {
        catalog_id_default(default, SqlDialect::Mysql, Some(expression_default))
    }
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
pub(crate) async fn snapshot_schema_for<D: SqlSession>(
    conn: &D,
    schema: &str,
) -> Result<SchemaSnapshot, DriftError> {
    let version_rows = conn
        .query("SELECT VERSION() AS server_version", &[])
        .await?;
    let server_version: String = version_rows
        .first()
        .ok_or_else(|| {
            DriftError::Snapshot("MySQL VERSION() returned no rows during drift snapshot".into())
        })?
        .try_get("server_version")?;
    let server_version = super::parse_mysql_version(&server_version)
        .map_err(|detail| DriftError::Snapshot(format!("MySQL drift snapshot: {detail}")))?;
    let supports_check_constraints = server_version >= [8, 0, 16];

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
            // COLUMN_TYPE is the modifier-bearing spelling. The numeric facets are
            // also available as their own catalog columns (CHARACTER_MAXIMUM_LENGTH,
            // NUMERIC_PRECISION, NUMERIC_SCALE, DATETIME_PRECISION) and SRS_ID is
            // available ONLY that way, because MySQL does not put a spatial column's
            // SRID in COLUMN_TYPE at all. Reading those instead of parsing is the
            // more robust source and is where this should go; parsing COLUMN_TYPE
            // covers everything except the SRID today, which is why `Spatial` is not
            // yet produced here.
            "SELECT TABLE_NAME AS table_name,
                    COLUMN_NAME AS column_name,
                    COLUMN_TYPE AS column_type,
                    CHARACTER_SET_NAME AS character_set_name,
                    COLLATION_NAME AS collation_name,
                    IS_NULLABLE AS is_nullable,
                    COLUMN_DEFAULT AS column_default,
                    EXTRA AS extra,
                    ORDINAL_POSITION AS ordinal_position
               FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ?
              ORDER BY TABLE_NAME, ORDINAL_POSITION",
            &[schema.into()],
        )
        .await?;
    let mut default_generated = BTreeMap::<(String, String), bool>::new();
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
        let default: Option<String> = row.try_get("column_default")?;
        let extra: String = row.try_get("extra")?;
        let column_name: String = row.try_get("column_name")?;
        default_generated.insert(
            (table_name.clone(), column_name.clone()),
            has_default_generated(&extra),
        );
        let identity = has_auto_increment(&extra).then_some(IdentityCol { always: false });
        let mysql_default_generated = default.as_ref().map(|_| has_default_generated(&extra));
        table.columns.push(ColumnSnapshot {
            name: column_name,
            data_type: crate::schema::query::mysql_canonical_type(&raw_type),
            mysql_physical_type: Some(MysqlPhysicalType::parse(&raw_type)),
            nullable: nullable.eq_ignore_ascii_case("YES"),
            default: default.clone(),
            ddl_type_override: None,
            inline_checks: Vec::new(),
            generated: None,
            identity,
            sqlite_rowid: false,
            value_format: None,
            // Set below from the CHECK pass, which is the only place the
            // engine's own UUID contract is recoverable on MySQL.
            catalog_uuid_format_check: false,
            id_default: identity.map(|_| {
                recover_mysql_id_default(default.as_deref(), has_default_generated(&extra), false)
            }),
            mysql_default_generated,
            case_sensitive: case_sensitive_from_collation(collation.as_deref())?,
            collation: None,
            mysql_text_storage,
            encryption_sentinel: None,
            comment_sentinel: None,
            comment: None,
        });
    }

    if supports_check_constraints {
        let check_rows = conn
            .query(
                "SELECT tc.TABLE_NAME AS table_name,
                        tc.CONSTRAINT_NAME AS constraint_name,
                        tc.ENFORCED AS enforced,
                        cc.CHECK_CLAUSE AS check_clause
                   FROM information_schema.TABLE_CONSTRAINTS tc
                   JOIN information_schema.CHECK_CONSTRAINTS cc
                     ON cc.CONSTRAINT_CATALOG = tc.CONSTRAINT_CATALOG
                    AND cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA
                    AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME
                  WHERE tc.CONSTRAINT_SCHEMA = ?
                    AND tc.CONSTRAINT_TYPE = 'CHECK'
                  ORDER BY tc.TABLE_NAME, tc.CONSTRAINT_NAME",
                &[schema.into()],
            )
            .await?;
        for row in check_rows {
            let table_name: String = row.try_get("table_name")?;
            let constraint_name: String = row.try_get("constraint_name")?;
            let enforced: String = row.try_get("enforced")?;
            if !enforced.eq_ignore_ascii_case("YES") {
                continue;
            }
            let check_clause: String = row.try_get("check_clause")?;
            let Some(table) = tables.get_mut(&table_name) else {
                return Err(DriftError::Snapshot(format!(
                    "MySQL catalog returned CHECK metadata for unknown table {table_name:?}"
                )));
            };

            let recovered = table
                .columns
                .iter()
                .enumerate()
                .filter_map(|(index, column)| {
                    recover_format_check(&column.name, &check_clause, SqlDialect::Mysql)
                        .map(|format| (index, format))
                })
                .collect::<Vec<_>>();
            if recovered.len() > 1 {
                return Err(DriftError::Snapshot(format!(
                    "MySQL catalog CHECK {table_name}.{constraint_name} ambiguously matches multiple ID-format columns"
                )));
            }
            let Some((column_index, recovered)) = recovered.into_iter().next() else {
                continue;
            };
            let column = &mut table.columns[column_index];
            if column.value_format.is_some() || column.id_default.is_some() {
                return Err(DriftError::Snapshot(format!(
                    "MySQL catalog returned multiple ID-format CHECKs for {table_name}.{}",
                    column.name
                )));
            }
            let expression_default = default_generated
                .get(&(table_name.clone(), column.name.clone()))
                .copied()
                .unwrap_or(false);
            column.id_default = Some(match &recovered {
                RecoveredFormatCheck::Uuid => {
                    recover_mysql_id_default(column.default.as_deref(), expression_default, true)
                }
                RecoveredFormatCheck::Value(_) => catalog_text_id_default(
                    column.default.as_deref(),
                    SqlDialect::Mysql,
                    Some(expression_default),
                ),
            });
            match recovered {
                // Retain the engine's own UUID contract as catalog evidence.
                // `value_format` cannot carry it (UUID is not a `ValueFormat`),
                // and the id-default classification above consumes it without
                // recording that the column enforces the UUID spelling locally.
                RecoveredFormatCheck::Uuid => column.catalog_uuid_format_check = true,
                RecoveredFormatCheck::Value(format) => column.value_format = Some(format),
            }
        }
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
                    cascade_columns: None,
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
                expr_cascade_columns: None,
            });
        }
    }

    let foreign_key_rows = conn
        .query(
            "SELECT kcu.TABLE_NAME AS table_name,
                    kcu.CONSTRAINT_NAME AS constraint_name,
                    kcu.ORDINAL_POSITION AS ordinal_position,
                    kcu.POSITION_IN_UNIQUE_CONSTRAINT AS position_in_unique_constraint,
                    kcu.COLUMN_NAME AS column_name,
                    kcu.REFERENCED_TABLE_SCHEMA AS referenced_table_schema,
                    kcu.REFERENCED_TABLE_NAME AS referenced_table_name,
                    kcu.REFERENCED_COLUMN_NAME AS referenced_column_name,
                    rc.UPDATE_RULE AS update_rule,
                    rc.DELETE_RULE AS delete_rule
               FROM information_schema.KEY_COLUMN_USAGE kcu
               JOIN information_schema.REFERENTIAL_CONSTRAINTS rc
                 ON rc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA
                AND rc.TABLE_NAME = kcu.TABLE_NAME
                AND rc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME
              WHERE kcu.CONSTRAINT_SCHEMA = ?
                AND kcu.REFERENCED_TABLE_NAME IS NOT NULL
              ORDER BY kcu.TABLE_NAME, kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
            &[schema.into()],
        )
        .await?;
    let mut foreign_keys = BTreeMap::<(String, String), ForeignKeyParts>::new();
    for row in foreign_key_rows {
        let table_name: String = row.try_get("table_name")?;
        if !tables.contains_key(&table_name) {
            return Err(DriftError::Snapshot(format!(
                "MySQL catalog returned foreign-key metadata for unknown table {table_name:?}"
            )));
        }
        let constraint_name: String = row.try_get("constraint_name")?;
        let ordinal: i64 = row.try_get("ordinal_position")?;
        let unique_position: i64 = row.try_get("position_in_unique_constraint")?;
        let column: String = row.try_get("column_name")?;
        let referenced_schema: String = row.try_get("referenced_table_schema")?;
        let referenced_table: String = row.try_get("referenced_table_name")?;
        let referenced_column: String = row.try_get("referenced_column_name")?;
        let raw_update_rule: String = row.try_get("update_rule")?;
        let raw_delete_rule: String = row.try_get("delete_rule")?;
        let update_rule = mysql_fk_action(&raw_update_rule)?.to_string();
        let delete_rule = mysql_fk_action(&raw_delete_rule)?.to_string();

        let key = (table_name.clone(), constraint_name.clone());
        let parts = foreign_keys
            .entry(key.clone())
            .or_insert_with(|| ForeignKeyParts {
                referenced_schema: referenced_schema.clone(),
                referenced_table: referenced_table.clone(),
                update_rule: update_rule.clone(),
                delete_rule: delete_rule.clone(),
                columns: Vec::new(),
                referenced_columns: Vec::new(),
            });
        let expected_ordinal = i64::try_from(parts.columns.len() + 1).unwrap_or(i64::MAX);
        if ordinal != expected_ordinal
            || unique_position != expected_ordinal
            || parts.referenced_schema != referenced_schema
            || parts.referenced_table != referenced_table
            || parts.update_rule != update_rule
            || parts.delete_rule != delete_rule
            || parts.columns.iter().any(|existing| existing == &column)
            || parts
                .referenced_columns
                .iter()
                .any(|existing| existing == &referenced_column)
        {
            return Err(DriftError::Snapshot(format!(
                "MySQL catalog returned inconsistent foreign-key metadata for {}.{} at ordinal {ordinal}",
                key.0, key.1
            )));
        }
        parts.columns.push(column);
        parts.referenced_columns.push(referenced_column);
    }

    for ((table_name, constraint_name), parts) in foreign_keys {
        let Some(table) = tables.get(&table_name) else {
            return Err(DriftError::Snapshot(format!(
                "MySQL catalog lost table {table_name:?} while assembling foreign key {constraint_name:?}"
            )));
        };
        if parts.columns.is_empty() || parts.columns.len() != parts.referenced_columns.len() {
            return Err(DriftError::Snapshot(format!(
                "MySQL catalog returned an empty or arity-mismatched foreign key {table_name}.{constraint_name}"
            )));
        }
        if parts.columns.iter().any(|column| {
            !table
                .columns
                .iter()
                .any(|candidate| candidate.name == *column)
        }) {
            return Err(DriftError::Snapshot(format!(
                "MySQL catalog foreign key {table_name}.{constraint_name} names an unknown local column"
            )));
        }
        if parts.referenced_schema.eq_ignore_ascii_case(schema) {
            let Some(referenced_table) = tables.get(&parts.referenced_table) else {
                return Err(DriftError::Snapshot(format!(
                    "MySQL catalog foreign key {table_name}.{constraint_name} names unknown referenced table {:?}",
                    parts.referenced_table
                )));
            };
            if parts.referenced_columns.iter().any(|column| {
                !referenced_table
                    .columns
                    .iter()
                    .any(|candidate| candidate.name == *column)
            }) {
                return Err(DriftError::Snapshot(format!(
                    "MySQL catalog foreign key {table_name}.{constraint_name} names an unknown referenced column"
                )));
            }
        }

        let constraint = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
            &parts.referenced_schema,
            &table_name,
            Some(&constraint_name),
            &parts.columns,
            &parts.referenced_table,
            &parts.referenced_columns,
            Some(&parts.delete_rule),
            Some(&parts.update_rule),
            false,
            false,
            crate::schema::query::SqlDialect::Mysql,
        );
        let table = tables
            .get_mut(&table_name)
            .expect("the immutable table lookup above proved this table exists");
        // A typed reference deliberately has no child format CHECK, so the FK is
        // the live evidence that its local column belongs to the ID-default
        // comparison surface. Promote its default-bearing local columns while
        // preserving MySQL's authoritative DEFAULT_GENERATED distinction: without
        // it, a literal string equal to the generator SQL could masquerade as the
        // expression.
        for column_name in &parts.columns {
            let column = table
                .columns
                .iter_mut()
                .find(|column| column.name == *column_name)
                .expect("the local-column validation above proved this column exists");
            if column.id_default.is_none() && column.default.is_some() {
                let expression_default = default_generated
                    .get(&(table_name.clone(), column.name.clone()))
                    .copied()
                    .unwrap_or(false);
                column.id_default = Some(if column.mysql_text_storage.is_some() {
                    catalog_text_id_default(
                        column.default.as_deref(),
                        SqlDialect::Mysql,
                        Some(expression_default),
                    )
                } else {
                    recover_mysql_id_default(column.default.as_deref(), expression_default, false)
                });
            }
        }
        table.constraints.push(constraint);
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

    // Six of the seven non-table maps are empty, for three different reasons. The
    // `..Default::default()` spelling makes them look identical, so the difference is
    // written out rather than left to be rediscovered.
    //
    // REFUSED - `model::dialect_table` records these as `Unsupported` on MySQL, so
    // `op_support::unsupported_reason` fires and no MySQL catalog can hold one:
    // `sequences` ("standalone sequence objects are PostgreSQL-only in the current
    // engine"), `schemas` ("schema vendor primitives are PostgreSQL-only"),
    // `extensions` ("extension vendor primitives are PostgreSQL-only"), and `roles`.
    // Populating them would be inventing readers for objects the engine will not author.
    //
    // AUTHORED BUT NOT A CATALOG OBJECT - `named_types`. `createEnum` and `createDomain`
    // are `Portable` on MySQL, so the engine does author them, but MySQL realizes an enum
    // as an inline column type rather than a schema object. The fold agrees: it gates the
    // snapshot insert on `Capability::MaterializedEnumType` (`render/fold.rs`), which only
    // PostgreSQL has, so a folded MySQL snapshot carries no named type either and the two
    // sides match.
    //
    // AUTHORED, COLLAPSED, AND THE FOLD DOES NOT AGREE - `partitions`. This one is not
    // refused: `createPartition` is `TransparentDegradable` on MySQL, and lowering
    // collapses the child into its parent behind a mirror guard rather than creating a
    // relation (`render/lower.rs`, "createPartition needs a collapse-affirmed parent on
    // SQLite/MySQL"). So an empty map is the right physical answer here. But the fold
    // inserts the child unconditionally, with none of the capability gate it applies to
    // named types, so a folded expected snapshot claims a relation this one cannot report
    // and the dialect-blind differ calls it missing. That gap is real and is tracked
    // separately; the map staying empty is not the half that is wrong.
    //
    // Views. `materialized` is false rather than defaulted: MySQL has none, and
    // `model::op_support` makes the materialized variants PostgreSQL-only, so no MySQL
    // catalog can hold one.
    //
    // `authored_query` and `authored_schema` stay `None`, which the field's own doc calls
    // for: "A snapshot built by introspection leaves it None, because a live catalog
    // cannot yield a typed query - and a drop with no typed body correctly stays
    // irreversible rather than guessing one." The typed body a `dropView` inverse needs
    // comes from replaying the authored history, not from here.
    //
    // `definition` is diagnostic only, per the same doc - PostgreSQL stores a bare SELECT
    // from `pg_get_viewdef` and SQLite the whole CREATE statement, so the text is already
    // not one format across backends and nothing executes it.
    let view_rows = conn
        .query(
            "SELECT TABLE_NAME AS view_name,
                    VIEW_DEFINITION AS view_definition
               FROM information_schema.VIEWS
              WHERE TABLE_SCHEMA = ?
              ORDER BY TABLE_NAME",
            &[schema.into()],
        )
        .await?;
    let mut views = BTreeMap::<String, ViewSnapshot>::new();
    for row in view_rows {
        let view_name: String = row.try_get("view_name")?;
        let definition: Option<String> = row.try_get("view_definition")?;
        views.insert(
            view_name,
            ViewSnapshot {
                materialized: false,
                columns: None,
                definition,
                authored_query: None,
                authored_schema: None,
                comment: None,
            },
        );
    }

    // POPULATED, and it was a real defect rather than a latent one. This map used to be
    // empty, with a comment here claiming nothing read it. The fold reads it: a
    // `dropView` naming a view an ALREADY-APPLIED migration created found nothing in the
    // projection and failed the deploy with `fold: view <name> does not exist`
    // (`render/fold.rs:2348`), because an applied migration's create carries journal
    // evidence and so is absent from `pending_ops` too. PostgreSQL and SQLite never had
    // it because both populate their views.
    Ok(SchemaSnapshot {
        tables,
        views,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        case_sensitive_from_collation, has_auto_increment, mysql_text_storage,
        recover_mysql_id_default,
    };
    use crate::model::snapshot::MysqlPhysicalType;

    // Every expectation below is a spelling read back from a live MySQL 8.4.11
    // catalog after creating the column, not a guess at how MySQL renders a type.
    #[test]
    fn a_varchar_keeps_the_length_the_portable_type_discards() {
        assert_eq!(
            MysqlPhysicalType::parse("varchar(64)"),
            MysqlPhysicalType::Character {
                fixed: false,
                length: 64
            }
        );
        assert_ne!(
            MysqlPhysicalType::parse("varchar(64)"),
            MysqlPhysicalType::parse("varchar(255)"),
            "the whole point is that these two stop comparing equal"
        );
    }

    #[test]
    fn a_decimal_is_not_told_apart_by_the_space_the_renderer_emits() {
        assert_eq!(
            MysqlPhysicalType::parse("decimal(65,30)"),
            MysqlPhysicalType::parse("DECIMAL(65, 30)"),
            "MySQL stores decimal(65,30) and the renderer emits DECIMAL(65, 30)"
        );
    }

    #[test]
    fn tinyint_1_is_the_boolean_and_a_wider_int_is_not() {
        assert_eq!(
            MysqlPhysicalType::parse("tinyint(1)"),
            MysqlPhysicalType::Integer {
                kind: "tinyint".to_string(),
                unsigned: false,
                boolean: true
            }
        );
        assert_eq!(
            MysqlPhysicalType::parse("int unsigned"),
            MysqlPhysicalType::Integer {
                kind: "int".to_string(),
                unsigned: true,
                boolean: false
            }
        );
    }

    #[test]
    fn enum_members_survive_their_interior_spaces_and_case() {
        assert_eq!(
            MysqlPhysicalType::parse("enum('a','b c')"),
            MysqlPhysicalType::Members {
                kind: "enum".to_string(),
                members: vec!["a".to_string(), "b c".to_string()]
            }
        );
        assert_eq!(
            MysqlPhysicalType::parse("enum('a','A')"),
            MysqlPhysicalType::Members {
                kind: "enum".to_string(),
                members: vec!["a".to_string(), "A".to_string()]
            },
            "a blanket lowercase would fold these two into one member"
        );
    }

    #[test]
    fn an_absent_temporal_precision_means_zero_rather_than_unknown() {
        assert_eq!(
            MysqlPhysicalType::parse("timestamp"),
            MysqlPhysicalType::Temporal {
                kind: "timestamp".to_string(),
                fsp: 0
            }
        );
        assert_eq!(
            MysqlPhysicalType::parse("datetime(3)"),
            MysqlPhysicalType::Temporal {
                kind: "datetime".to_string(),
                fsp: 3
            }
        );
    }

    #[test]
    fn an_unmodelled_family_is_named_rather_than_guessed() {
        assert_eq!(
            MysqlPhysicalType::parse("point"),
            MysqlPhysicalType::Unknown {
                raw: "point".to_string()
            },
            "spatial carries an SRID that COLUMN_TYPE does not, so it is not modelled here yet"
        );
    }
    use crate::model::snapshot::IdDefaultSnapshot;

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

    #[test]
    fn mysql_generation_metadata_recovers_auto_increment_and_deparsed_uuid_v4() {
        assert!(has_auto_increment("DEFAULT_GENERATED auto_increment"));
        assert!(!has_auto_increment("DEFAULT_GENERATED"));

        // MySQL 8.4's information_schema spelling adds grouping parentheses
        // and character-set introducers to the authored renderer expression.
        let catalog = "lower(concat(hex(random_bytes(4)),_latin1'-',hex(random_bytes(2)),_latin1'-',hex(((ord(random_bytes(1)) & 15) | 64)),hex(random_bytes(1)),_latin1'-',hex(((ord(random_bytes(1)) & 63) | 128)),hex(random_bytes(1)),_latin1'-',hex(random_bytes(6))))";
        assert_eq!(
            recover_mysql_id_default(Some(catalog), true, true),
            IdDefaultSnapshot::UuidV4
        );
        assert_eq!(
            recover_mysql_id_default(Some("uuid()"), true, true),
            IdDefaultSnapshot::Expression(
                crate::render::value_format::catalog_expression_fingerprint("uuid()"),
            )
        );
        assert_eq!(
            recover_mysql_id_default(Some("uuid()"), false, true),
            IdDefaultSnapshot::Literal("\"uuid()\"".to_string())
        );
        assert_eq!(
            recover_mysql_id_default(None, false, true),
            IdDefaultSnapshot::Absent
        );
    }
}
