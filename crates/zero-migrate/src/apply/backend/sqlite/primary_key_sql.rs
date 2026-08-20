//! Apply-time SQLite primary-key lifecycle validation and rebuild planning.

use std::collections::BTreeMap;

use crate::model::ir::AlterPrimaryKeyAction;
use crate::render::plan::{SqliteRebuildSpec, SqliteSequencePolicy};

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;

#[derive(Debug, Clone)]
struct Column {
    name: String,
    declared_type: String,
    not_null: bool,
    pk_ordinal: i64,
}

fn fail(message: impl Into<String>) -> SqliteActorError {
    SqliteActorError::Exec(format!(
        "explicit primary-key lifecycle precondition failed: {}",
        message.into()
    ))
}

fn lit(value: &str) -> String {
    super::journal_sql::sql_lit(value)
}

fn ident(value: &str) -> String {
    crate::render::dml::escape_quote_ident_for_dialect(value, super::SQLITE_DIALECT)
}

fn cell(row: &[Option<String>], index: usize, field: &str) -> Result<String, SqliteActorError> {
    row.get(index)
        .and_then(Clone::clone)
        .ok_or_else(|| fail(format!("SQLite catalog omitted {field}")))
}

fn integer_cell(row: &[Option<String>], index: usize) -> i64 {
    row.get(index)
        .and_then(Clone::clone)
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_default()
}

async fn table_columns(
    actor: &MigrationActor,
    table: &str,
) -> Result<Vec<Column>, SqliteActorError> {
    let rows = actor
        .query(&format!("PRAGMA main.table_info({})", lit(table)))
        .await?;
    if rows.is_empty() {
        return Err(fail(format!("table {table:?} does not exist")));
    }
    rows.into_iter()
        .map(|row| {
            Ok(Column {
                name: cell(&row, 1, "table_info.name")?,
                declared_type: row.get(2).and_then(Clone::clone).unwrap_or_default(),
                not_null: integer_cell(&row, 3) == 1,
                pk_ordinal: integer_cell(&row, 5),
            })
        })
        .collect()
}

fn primary_key(columns: &[Column]) -> Vec<String> {
    let mut members = columns
        .iter()
        .filter(|column| column.pk_ordinal > 0)
        .map(|column| (column.pk_ordinal, column.name.clone()))
        .collect::<Vec<_>>();
    members.sort_by_key(|(ordinal, _)| *ordinal);
    members.into_iter().map(|(_, name)| name).collect()
}

async fn has_exact_unique_key(
    actor: &MigrationActor,
    table: &str,
    columns: &[String],
    stored_create: &str,
) -> Result<bool, SqliteActorError> {
    let indexes = actor
        .query(&format!("PRAGMA main.index_list({})", lit(table)))
        .await?;
    for index in indexes {
        let unique = integer_cell(&index, 2) == 1;
        let origin = index.get(3).and_then(Clone::clone).unwrap_or_default();
        let partial = integer_cell(&index, 4) == 1;
        if !unique || partial || origin.eq_ignore_ascii_case("pk") {
            continue;
        }
        let name = cell(&index, 1, "index_list.name")?;
        let info = actor
            .query(&format!("PRAGMA main.index_xinfo({})", lit(&name)))
            .await?;
        let mut keyed = info
            .into_iter()
            .filter(|row| integer_cell(row, 5) == 1)
            .collect::<Vec<_>>();
        keyed.sort_by_key(|row| integer_cell(row, 0));
        if keyed.len() != columns.len() {
            continue;
        }
        let mut exact = true;
        for (offset, (row, expected)) in keyed.iter().zip(columns).enumerate() {
            if integer_cell(row, 0) != i64::try_from(offset).unwrap_or(i64::MAX)
                || integer_cell(row, 1) < 0
            {
                exact = false;
                break;
            }
            let Some(actual) = row.get(2).and_then(Clone::clone) else {
                exact = false;
                break;
            };
            if !actual.eq_ignore_ascii_case(expected) {
                exact = false;
                break;
            }
            let actual_collation = row
                .get(4)
                .and_then(Clone::clone)
                .unwrap_or_else(|| "BINARY".to_string());
            let expected_collation =
                super::drift_sql::sqlite_column_collation_name(stored_create, expected)
                    .unwrap_or_else(|| "BINARY".to_string());
            if !actual_collation.eq_ignore_ascii_case(&expected_collation) {
                exact = false;
                break;
            }
        }
        if exact {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn has_primary_key_index(
    actor: &MigrationActor,
    table: &str,
) -> Result<bool, SqliteActorError> {
    Ok(actor
        .query(&format!("PRAGMA main.index_list({})", lit(table)))
        .await?
        .iter()
        .any(|index| {
            index
                .get(3)
                .and_then(Clone::clone)
                .is_some_and(|origin| origin.eq_ignore_ascii_case("pk"))
        }))
}

async fn verify_inbound_foreign_keys(
    actor: &MigrationActor,
    table: &str,
    removed_key: &[String],
    stored_create: &str,
) -> Result<(), SqliteActorError> {
    let alternate = has_exact_unique_key(actor, table, removed_key, stored_create).await?;
    let tables = actor
        .query(
            "SELECT name FROM main.sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .await?;
    for row in tables {
        let child = cell(&row, 0, "sqlite_master.name")?;
        let foreign_keys = actor
            .query(&format!("PRAGMA main.foreign_key_list({})", lit(&child)))
            .await?;
        let mut grouped: BTreeMap<i64, Vec<(i64, Option<String>)>> = BTreeMap::new();
        for foreign_key in foreign_keys {
            let referenced_table = foreign_key
                .get(2)
                .and_then(Clone::clone)
                .unwrap_or_default();
            if !referenced_table.eq_ignore_ascii_case(table) {
                continue;
            }
            grouped
                .entry(integer_cell(&foreign_key, 0))
                .or_default()
                .push((
                    integer_cell(&foreign_key, 1),
                    foreign_key.get(4).and_then(Clone::clone),
                ));
        }
        for (_id, mut members) in grouped {
            members.sort_by_key(|(sequence, _)| *sequence);
            let implicit_primary_key = members.iter().all(|(_, column)| {
                column
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
            });
            let referenced = members
                .iter()
                .filter_map(|(_, column)| column.clone())
                .collect::<Vec<_>>();
            if implicit_primary_key {
                return Err(fail(format!(
                    "cannot replace or drop primary key ({}) on {table:?}: implicit inbound foreign key from {child:?} follows the table primary key and this operation does not migrate foreign keys",
                    removed_key.join(", ")
                )));
            }
            let uses_removed_key = referenced.len() == removed_key.len()
                && referenced
                    .iter()
                    .zip(removed_key)
                    .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected));
            if uses_removed_key && !alternate {
                return Err(fail(format!(
                    "cannot remove primary key ({}) from {table:?}: inbound foreign key from {child:?} has no exact alternate UNIQUE key",
                    removed_key.join(", ")
                )));
            }
        }
    }
    Ok(())
}

fn target_columns(action: &AlterPrimaryKeyAction) -> Option<&[String]> {
    match action {
        AlterPrimaryKeyAction::Add { columns } | AlterPrimaryKeyAction::Replace { columns, .. } => {
            Some(columns)
        }
        AlterPrimaryKeyAction::Drop { .. } => None,
    }
}

/// Resolve and validate one operation into the existing hardened rebuild shape.
pub(crate) async fn resolve(
    actor: &MigrationActor,
    schema: &str,
    table: &str,
    action: &AlterPrimaryKeyAction,
) -> Result<SqliteRebuildSpec, SqliteActorError> {
    let _ = schema;
    actor.set_mode(Mode::EngineJournal).await?;
    let create_rows = actor
        .query(&format!(
            "SELECT sql FROM main.sqlite_master WHERE type = 'table' AND name = {}",
            lit(table)
        ))
        .await?;
    let stored_create = create_rows
        .first()
        .and_then(|row| row.first())
        .and_then(Clone::clone)
        .ok_or_else(|| fail(format!("table {table:?} has no stored CREATE TABLE")))?;
    let columns = table_columns(actor, table).await?;
    let current = primary_key(&columns);
    let without_rowid = crate::render::declarative::sqlite_create_is_without_rowid(&stored_create);

    match action {
        AlterPrimaryKeyAction::Add { .. } if !current.is_empty() => {
            return Err(fail(format!(
                "add expected no current primary key on {table:?}, found ({})",
                current.join(", ")
            )));
        }
        AlterPrimaryKeyAction::Replace {
            expected_columns, ..
        }
        | AlterPrimaryKeyAction::Drop {
            expected_columns, ..
        } if current != *expected_columns => {
            return Err(fail(format!(
                "expected current primary key ({}) on {table:?}, found ({})",
                expected_columns.join(", "),
                current.join(", ")
            )));
        }
        _ => {}
    }

    if without_rowid && matches!(action, AlterPrimaryKeyAction::Drop { .. }) {
        return Err(fail(format!(
            "cannot drop the primary key from WITHOUT ROWID table {table:?}"
        )));
    }

    // A true rowid alias is the sole exact INTEGER primary key and has no
    // separate `origin='pk'` index. This catalog distinction excludes both
    // WITHOUT ROWID tables and SQLite's historical `INTEGER PRIMARY KEY DESC`
    // exception, neither of which owns automatic rowid generation.
    let primary_has_index = has_primary_key_index(actor, table).await?;
    let generated_rowid = if current.len() == 1 && !without_rowid && !primary_has_index {
        columns.iter().find(|column| {
            column.name.eq_ignore_ascii_case(&current[0])
                && column.declared_type.trim().eq_ignore_ascii_case("INTEGER")
        })
    } else {
        None
    };

    let drop_identity_from = action.drop_identity_from();
    for declared in drop_identity_from {
        if !generated_rowid.is_some_and(|column| column.name.eq_ignore_ascii_case(declared)) {
            return Err(fail(format!(
                "dropIdentityFrom column {declared:?} is not the live generated INTEGER PRIMARY KEY"
            )));
        }
    }
    if let Some(identity) = generated_rowid {
        let declared = drop_identity_from
            .iter()
            .any(|column| column.eq_ignore_ascii_case(&identity.name));
        if !matches!(action, AlterPrimaryKeyAction::Add { .. }) && !declared {
            return Err(fail(format!(
                "generated INTEGER PRIMARY KEY column {:?} loses its generation contract; list it in dropIdentityFrom",
                identity.name
            )));
        }
    }

    if let Some(target) = target_columns(action) {
        let would_introduce_rowid_generation = !without_rowid
            && target.len() == 1
            && columns.iter().any(|column| {
                column.name.eq_ignore_ascii_case(&target[0])
                    && column.declared_type.trim().eq_ignore_ascii_case("INTEGER")
            });
        if would_introduce_rowid_generation {
            return Err(fail(format!(
                "target primary key ({}) would introduce SQLite INTEGER PRIMARY KEY rowid generation, but add/replace may only remove generation via dropIdentityFrom",
                target.join(", ")
            )));
        }
        for name in target {
            let column = columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    fail(format!("target primary-key column {name:?} does not exist"))
                })?;
            let semantically_not_null = column.not_null
                || generated_rowid
                    .is_some_and(|identity| identity.name.eq_ignore_ascii_case(&column.name));
            if !semantically_not_null {
                return Err(fail(format!(
                    "target primary-key column {name:?} must already be NOT NULL"
                )));
            }
        }
        if !has_exact_unique_key(actor, table, target, &stored_create).await? {
            return Err(fail(format!(
                "target primary key ({}) requires an exact pre-existing UNIQUE key",
                target.join(", ")
            )));
        }
    }

    if !matches!(action, AlterPrimaryKeyAction::Add { .. }) {
        verify_inbound_foreign_keys(actor, table, &current, &stored_create).await?;
    }

    let rewritten = crate::render::declarative::rewrite_sqlite_stored_primary_key(
        table,
        &stored_create,
        target_columns(action),
        generated_rowid.map(|column| column.name.as_str()),
    )
    .map_err(|error| fail(error.to_string()))?;
    let (open, _) = crate::render::declarative::sqlite_create_body_bounds(&rewritten)
        .ok_or_else(|| fail("rewritten CREATE TABLE has no body"))?;
    let tmp_table = SqliteRebuildSpec::tmp_name(table);
    let new_table_create = format!("CREATE TABLE {} {}", ident(&tmp_table), &rewritten[open..]);
    let generated = crate::render::declarative::sqlite_generated_columns(&rewritten);
    let copy_columns = columns
        .iter()
        .filter(|column| {
            !generated
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&column.name))
        })
        .map(|column| (column.name.clone(), column.name.clone()))
        .collect();

    Ok(SqliteRebuildSpec {
        table: table.to_string(),
        tmp_table,
        new_table_create,
        copy_columns,
        recreate_objects: Vec::new(),
        column_renames: Vec::new(),
        dropped_columns: Vec::new(),
        reason: "explicit primary-key lifecycle operation".to_string(),
        sequence_policy: if drop_identity_from.is_empty() {
            SqliteSequencePolicy::Preserve
        } else {
            SqliteSequencePolicy::Remove
        },
    })
}
