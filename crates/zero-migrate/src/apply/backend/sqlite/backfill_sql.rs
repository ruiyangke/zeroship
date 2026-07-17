//! Crash-safe SQLite backfills over planner-proven ordered cursor tuples.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::apply::backend::{BackfillError, BackfillOutcome, BackfillProgressEntry};
use crate::model::backfill::{
    generate_per_row_value, BackfillSpec, CursorColumnContract, CursorComparison, CursorContract,
    CursorScalarType, CursorTuple,
};
use crate::model::ir::{CursorStability, IrScalar, PerRowGenerator};
use crate::model::migration::{Checksum, MigrationId};
use crate::render::dml::sqlite_placeholder;

use super::actor::{MigrationActor, SqliteActorError, SqliteBind};
use super::authorizer::Mode;
use super::journal_sql::sql_lit;

#[derive(Clone, Copy)]
pub(crate) struct PlanBackfillIdentity<'a> {
    pub(crate) version: &'a MigrationId,
    pub(crate) checksum: &'a Checksum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SqliteCursorKind {
    Integer,
    Text,
}

impl SqliteCursorKind {
    const fn storage_class(self) -> &'static str {
        match self {
            Self::Integer => "integer",
            Self::Text => "text",
        }
    }

    const fn scalar_type(self) -> CursorScalarType {
        match self {
            Self::Integer => CursorScalarType::Int64,
            Self::Text => CursorScalarType::String,
        }
    }
}

#[derive(Debug)]
struct LiveCursorContract {
    contract: CursorContract,
    kinds: Vec<SqliteCursorKind>,
}

#[derive(Debug)]
struct TableColumn {
    name: String,
    database_type: String,
    not_null: bool,
    pk_ordinal: usize,
    kind: Option<SqliteCursorKind>,
}

#[derive(Debug)]
struct IndexCandidate {
    primary: bool,
    columns: Vec<String>,
    collations: Vec<String>,
}

#[derive(Debug, Clone)]
struct Progress {
    checksum: String,
    target_table: String,
    cursor_columns_json: String,
    cursor_contract_json: String,
    stability_mode: String,
    stability_name: Option<String>,
    guard_name: Option<String>,
    guard_installed: bool,
    guard_cleaned: bool,
    last_cursor_json: Option<String>,
    end_cursor_json: Option<String>,
    cohort_checksum: String,
    cohort_initialized: bool,
    complete: bool,
    exists: bool,
}

const PROGRESS_COLUMNS: &[&str] = &[
    "backfill_id",
    "checksum",
    "name",
    "target_table",
    "cursor_columns",
    "cursor_contract",
    "stability_mode",
    "stability_name",
    "guard_name",
    "guard_installed",
    "guard_cleaned",
    "last_cursor",
    "end_cursor",
    "cohort_checksum",
    "cohort_initialized",
    "rows_done",
    "batches_done",
    "complete",
    "applied_by",
    "started_at",
    "updated_at",
];

fn validate_ident(what: &'static str, value: &str) -> Result<(), BackfillError> {
    let valid = !value.is_empty()
        && value.starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_');
    if valid {
        Ok(())
    } else {
        Err(BackfillError::InvalidIdentifier {
            what,
            value: value.to_string(),
        })
    }
}

fn quote_ident(identifier: &str) -> String {
    crate::render::dml::escape_quote_ident(identifier)
}

fn cursor_unavailable(spec: &BackfillSpec, reason: impl Into<String>) -> BackfillError {
    BackfillError::CursorTupleUnavailable {
        table: spec.table.clone(),
        cursor_columns: spec.cursor_columns.clone(),
        reason: reason.into(),
    }
}

fn validate_spec(spec: &BackfillSpec, set_clause: &str) -> Result<(), BackfillError> {
    validate_ident("table", &spec.table)?;
    if spec.cursor_columns.is_empty() {
        return Err(cursor_unavailable(
            spec,
            "the ordered cursor tuple is empty",
        ));
    }
    let mut seen = BTreeSet::new();
    for column in &spec.cursor_columns {
        validate_ident("cursor component", column)?;
        let folded = column.to_ascii_lowercase();
        if !seen.insert(folded) {
            return Err(cursor_unavailable(
                spec,
                format!("cursor component {column:?} appears more than once"),
            ));
        }
    }
    if spec.batch_size == 0 {
        return Err(BackfillError::InvalidBatchSize);
    }
    if set_clause.trim().is_empty() && spec.per_row.is_empty() {
        return Err(BackfillError::InvalidSpec(
            "backfill set must not be empty".to_string(),
        ));
    }
    if let CursorStability::ExternalInvariant { name } = &spec.cursor_stability {
        if name.trim().is_empty() {
            return Err(BackfillError::InvalidSpec(
                "external cursor invariant name must be non-empty".to_string(),
            ));
        }
    }
    for column in &spec.cursor_columns {
        assert_component_not_mutated(set_clause, column)?;
    }
    for (column, assignment) in &spec.per_row {
        validate_ident("per-row destination column", column)?;
        if spec
            .cursor_columns
            .iter()
            .any(|cursor| cursor.eq_ignore_ascii_case(column))
        {
            return Err(BackfillError::CursorComponentMutated {
                cursor_component: column.clone(),
            });
        }
        if !assignment.matches_target(&spec.schema, &spec.table, column) {
            return Err(BackfillError::InvalidSpec(format!(
                "per-row assignment for destination {column:?} was validated for a different target"
            )));
        }
        if let PerRowGenerator::TypeId { prefix } = assignment.generator() {
            crate::model::ir::validate_type_id_prefix(prefix).map_err(|error| {
                BackfillError::InvalidSpec(format!(
                    "invalid TypeID prefix for per-row destination {column:?}: {error}"
                ))
            })?;
        }
    }
    if let Some(contract) = &spec.cursor_contract {
        contract
            .validate_columns(&spec.cursor_columns)
            .map_err(|error| BackfillError::InvalidSpec(error.to_string()))?;
    }
    Ok(())
}

fn assert_component_not_mutated(
    set_clause: &str,
    cursor_component: &str,
) -> Result<(), BackfillError> {
    let needle = format!("{} =", quote_ident(cursor_component));
    let bytes = set_clause.as_bytes();
    let mut index = 0;
    let mut in_string = false;
    let mut at_assignment = true;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if byte == b'\'' {
                if index + 1 < bytes.len() && bytes[index + 1] == b'\'' {
                    index += 2;
                    continue;
                }
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'\'' => {
                in_string = true;
                at_assignment = false;
                index += 1;
            }
            b',' => {
                at_assignment = true;
                index += 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' => index += 1,
            _ => {
                if at_assignment && set_clause[index..].starts_with(&needle) {
                    return Err(BackfillError::CursorComponentMutated {
                        cursor_component: cursor_component.to_string(),
                    });
                }
                at_assignment = false;
                index += 1;
            }
        }
    }
    Ok(())
}

fn sqlite_cursor_kind(database_type: &str) -> Option<SqliteCursorKind> {
    let upper = database_type.to_ascii_uppercase();
    if upper.contains("INT") {
        Some(SqliteCursorKind::Integer)
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        Some(SqliteCursorKind::Text)
    } else {
        None
    }
}

fn normalized_database_type(database_type: &str) -> String {
    database_type
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn sqlite_comparison(collation: &str) -> Result<CursorComparison, String> {
    if collation.eq_ignore_ascii_case("BINARY") {
        Ok(CursorComparison::Default)
    } else if collation.eq_ignore_ascii_case("NOCASE") {
        Ok(CursorComparison::CaseInsensitive)
    } else if collation.eq_ignore_ascii_case("RTRIM") {
        Ok(CursorComparison::NamedCollation {
            schema: None,
            name: "RTRIM".to_string(),
        })
    } else {
        Err(format!(
            "SQLite collation {collation:?} is not one of the built-in comparison semantics the executor can prove"
        ))
    }
}

async fn table_columns(
    actor: &MigrationActor,
    spec: &BackfillSpec,
) -> Result<Vec<TableColumn>, BackfillError> {
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(sqlite_journal_err)?;
    let rows = actor
        .query(&format!("PRAGMA table_info({})", quote_ident(&spec.table)))
        .await
        .map_err(sqlite_journal_err)?;
    if rows.is_empty() {
        return Err(BackfillError::TargetNotFound(format!(
            "table {:?} was not found",
            spec.table
        )));
    }
    rows.into_iter()
        .map(|row| {
            let name = required_cell(&row, 1, "table_info.name")?;
            let database_type = required_cell(&row, 2, "table_info.type")?;
            let declared_not_null = required_cell(&row, 3, "table_info.notnull")? == "1";
            let pk_ordinal = required_cell(&row, 5, "table_info.pk")?
                .parse::<usize>()
                .map_err(|_| {
                    sqlite_journal_err(SqliteActorError::Exec(
                        "PRAGMA table_info returned an invalid primary-key ordinal".to_string(),
                    ))
                })?;
            let kind = sqlite_cursor_kind(&database_type);
            Ok(TableColumn {
                name,
                database_type: normalized_database_type(&database_type),
                not_null: declared_not_null,
                pk_ordinal,
                kind,
            })
        })
        .collect()
}

async fn index_candidates(
    actor: &MigrationActor,
    spec: &BackfillSpec,
) -> Result<Vec<IndexCandidate>, BackfillError> {
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(sqlite_journal_err)?;
    let rows = actor
        .query(&format!("PRAGMA index_list({})", quote_ident(&spec.table)))
        .await
        .map_err(sqlite_journal_err)?;
    let mut candidates = Vec::new();
    for row in rows {
        let name = required_cell(&row, 1, "index_list.name")?;
        let unique = required_cell(&row, 2, "index_list.unique")? == "1";
        let origin = required_cell(&row, 3, "index_list.origin")?;
        let partial = required_cell(&row, 4, "index_list.partial")? == "1";
        if !unique || partial {
            continue;
        }
        let parts = actor
            .query(&format!("PRAGMA index_xinfo({})", quote_ident(&name)))
            .await
            .map_err(sqlite_journal_err)?;
        let mut keyed = Vec::new();
        let mut usable = true;
        for part in parts {
            if required_cell(&part, 5, "index_xinfo.key")? != "1" {
                continue;
            }
            let sequence = required_cell(&part, 0, "index_xinfo.seqno")?
                .parse::<usize>()
                .map_err(|_| {
                    sqlite_journal_err(SqliteActorError::Exec(
                        "PRAGMA index_xinfo returned an invalid key ordinal".to_string(),
                    ))
                })?;
            let cid = required_cell(&part, 1, "index_xinfo.cid")?
                .parse::<i64>()
                .unwrap_or(-2);
            let Some(column) = part.get(2).and_then(Clone::clone) else {
                usable = false;
                break;
            };
            if cid < 0 {
                usable = false;
                break;
            }
            let collation = part
                .get(4)
                .and_then(Clone::clone)
                .unwrap_or_else(|| "BINARY".to_string());
            keyed.push((sequence, column, collation));
        }
        if usable && !keyed.is_empty() {
            keyed.sort_by_key(|(sequence, _, _)| *sequence);
            let (columns, collations): (Vec<_>, Vec<_>) = keyed
                .into_iter()
                .map(|(_, column, collation)| (column, collation))
                .unzip();
            candidates.push(IndexCandidate {
                primary: origin.eq_ignore_ascii_case("pk"),
                columns,
                collations,
            });
        }
    }
    candidates.sort_by_key(|candidate| !candidate.primary);
    Ok(candidates)
}

fn tuple_names_match(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

async fn resolve_live_cursor_contract(
    actor: &MigrationActor,
    spec: &BackfillSpec,
) -> Result<LiveCursorContract, BackfillError> {
    let columns = table_columns(actor, spec).await?;
    let by_name = columns
        .iter()
        .map(|column| (column.name.to_ascii_lowercase(), column))
        .collect::<BTreeMap<_, _>>();
    let mut selected = Vec::with_capacity(spec.cursor_columns.len());
    for name in &spec.cursor_columns {
        let Some(column) = by_name.get(&name.to_ascii_lowercase()).copied() else {
            return Err(BackfillError::TargetNotFound(format!(
                "table {:?} has no cursor component {name:?}",
                spec.table
            )));
        };
        if column.kind.is_none() {
            return Err(cursor_unavailable(
                spec,
                format!(
                    "cursor component {name:?} has unsupported SQLite type {:?}; only exact INTEGER and TEXT domains are resumable",
                    column.database_type
                ),
            ));
        }
        selected.push(column);
    }

    let mut primary_columns = columns
        .iter()
        .filter(|column| column.pk_ordinal > 0)
        .collect::<Vec<_>>();
    primary_columns.sort_by_key(|column| column.pk_ordinal);
    let primary_names = primary_columns
        .iter()
        .map(|column| column.name.clone())
        .collect::<Vec<_>>();
    let candidates = index_candidates(actor, spec).await?;
    let has_primary_index = candidates.iter().any(|candidate| {
        candidate.primary && tuple_names_match(&candidate.columns, &spec.cursor_columns)
    });
    // SQLite's rowid alias is the one narrow PRIMARY KEY exception where
    // `table_info.notnull` remains zero despite nulls being impossible. Prove it
    // from the sole exact `INTEGER` declaration and the absence of a physical PK
    // index. BIGINT aliases, composite PK first components, and the historical
    // `INTEGER PRIMARY KEY DESC` form all fail this test.
    let integer_rowid_alias = tuple_names_match(&primary_names, &spec.cursor_columns)
        && selected.len() == 1
        && selected[0].database_type == "integer"
        && !has_primary_index;
    for (name, column) in spec.cursor_columns.iter().zip(&selected) {
        if !column.not_null && !integer_rowid_alias {
            return Err(cursor_unavailable(
                spec,
                format!("cursor component {name:?} is nullable"),
            ));
        }
    }
    let index_match = candidates
        .iter()
        .find(|candidate| tuple_names_match(&candidate.columns, &spec.cursor_columns));
    let collations = if integer_rowid_alias {
        vec!["BINARY".to_string()]
    } else if let Some(candidate) = index_match {
        candidate.collations.clone()
    } else {
        return Err(cursor_unavailable(
            spec,
            "the declared tuple is not the complete ordered key of a PRIMARY KEY or non-partial UNIQUE index",
        ));
    };

    let mut contract_columns = Vec::with_capacity(selected.len());
    let mut kinds = Vec::with_capacity(selected.len());
    for ((authored_name, column), collation) in
        spec.cursor_columns.iter().zip(selected).zip(collations)
    {
        let kind = column.kind.expect("validated cursor kind");
        let comparison =
            sqlite_comparison(&collation).map_err(|reason| cursor_unavailable(spec, reason))?;
        contract_columns.push(CursorColumnContract {
            name: authored_name.clone(),
            scalar_type: kind.scalar_type(),
            // Persist the semantic cursor family, not SQLite's arbitrary
            // declared-type alias. BIGINT, UNSIGNED BIG INT, and INTEGER share
            // one exact integer codec; VARCHAR/CLOB/TEXT share one text codec.
            database_type: kind.storage_class().to_string(),
            comparison,
        });
        kinds.push(kind);
    }
    let live = LiveCursorContract {
        contract: CursorContract {
            columns: contract_columns,
        },
        kinds,
    };
    if let Some(planned) = &spec.cursor_contract {
        if planned != &live.contract {
            return Err(cursor_unavailable(
                spec,
                format!(
                    "cursor type/collation contract drifted: planned {planned:?}, live {:?}",
                    live.contract
                ),
            ));
        }
    }
    Ok(live)
}

async fn validate_storage_classes(
    actor: &MigrationActor,
    spec: &BackfillSpec,
    live: &LiveCursorContract,
) -> Result<(), BackfillError> {
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(sqlite_journal_err)?;
    let table = quote_ident(&spec.table);
    for (column, kind) in spec.cursor_columns.iter().zip(&live.kinds) {
        let quoted = quote_ident(column);
        let classes = actor
            .query(&format!(
                "SELECT DISTINCT typeof({quoted}) FROM {table} ORDER BY 1"
            ))
            .await
            .map_err(sqlite_journal_err)?;
        if classes
            .iter()
            .any(|row| row.first().and_then(|cell| cell.as_deref()) != Some(kind.storage_class()))
        {
            return Err(cursor_unavailable(
                spec,
                format!(
                    "cursor component {column:?} contains a runtime storage class other than {:?}",
                    kind.storage_class()
                ),
            ));
        }
        if *kind == SqliteCursorKind::Text {
            actor
                .validate_text_utf8(&format!("SELECT {quoted} FROM {table}"))
                .await
                .map_err(sqlite_journal_err)?;
        }
    }
    Ok(())
}

fn required_cell(
    row: &[Option<String>],
    index: usize,
    what: &str,
) -> Result<String, BackfillError> {
    row.get(index).and_then(Clone::clone).ok_or_else(|| {
        sqlite_journal_err(SqliteActorError::Exec(format!(
            "SQLite catalog returned null {what}"
        )))
    })
}

fn cursor_projection(columns: &[String]) -> String {
    columns
        .iter()
        .flat_map(|column| {
            let quoted = quote_ident(column);
            [quoted.clone(), format!("typeof({quoted})")]
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn key_projection(columns: &[String]) -> String {
    columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ")
}

fn comparison_expression(column: &CursorColumnContract) -> String {
    let collation = match &column.comparison {
        CursorComparison::Default => "BINARY",
        CursorComparison::CaseInsensitive => "NOCASE",
        CursorComparison::NamedCollation { schema: None, name } => name,
        CursorComparison::NamedCollation {
            schema: Some(_), ..
        }
        | CursorComparison::MysqlText { .. } => {
            unreachable!("non-SQLite comparison escaped live-contract validation")
        }
    };
    format!(
        "{} COLLATE {}",
        quote_ident(&column.name),
        quote_ident(collation)
    )
}

fn order_by(columns: &[CursorColumnContract], descending: bool) -> String {
    let direction = if descending { "DESC" } else { "ASC" };
    columns
        .iter()
        .map(|column| format!("{} {direction}", comparison_expression(column)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn lexicographic_after(columns: &[CursorColumnContract], first_placeholder: usize) -> String {
    lexicographic_comparison(columns, first_placeholder, false)
}

fn lexicographic_at_or_before(
    columns: &[CursorColumnContract],
    first_placeholder: usize,
) -> String {
    lexicographic_comparison(columns, first_placeholder, true)
}

fn lexicographic_comparison(
    columns: &[CursorColumnContract],
    first_placeholder: usize,
    upper_bound: bool,
) -> String {
    let mut terms = Vec::with_capacity(columns.len());
    for index in 0..columns.len() {
        let mut parts = Vec::with_capacity(index + 1);
        for (prefix, column) in columns.iter().take(index).enumerate() {
            parts.push(format!(
                "{} = {}",
                comparison_expression(column),
                sqlite_placeholder(first_placeholder + prefix)
            ));
        }
        let operator = if upper_bound {
            if index + 1 == columns.len() {
                "<="
            } else {
                "<"
            }
        } else {
            ">"
        };
        parts.push(format!(
            "{} {operator} {}",
            comparison_expression(&columns[index]),
            sqlite_placeholder(first_placeholder + index)
        ));
        terms.push(format!("({})", parts.join(" AND ")));
    }
    format!("({})", terms.join(" OR "))
}

fn window_predicate(
    columns: &[CursorColumnContract],
    filter: Option<&str>,
    have_last: bool,
) -> (String, usize) {
    let arity = columns.len();
    let mut predicates = Vec::new();
    let end_first = if have_last {
        predicates.push(lexicographic_after(columns, 1));
        arity + 1
    } else {
        1
    };
    predicates.push(lexicographic_at_or_before(columns, end_first));
    if let Some(filter) = filter {
        predicates.push(format!("({filter})"));
    }
    (predicates.join(" AND "), end_first + arity)
}

fn build_window_sql(
    table: &str,
    contract: &CursorContract,
    filter: Option<&str>,
    have_last: bool,
) -> String {
    let columns = contract
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect::<Vec<_>>();
    let (predicate, limit_placeholder) = window_predicate(&contract.columns, filter, have_last);
    format!(
        "SELECT {} FROM {} WHERE {predicate} ORDER BY {} LIMIT {}",
        cursor_projection(&columns),
        quote_ident(table),
        order_by(&contract.columns, false),
        sqlite_placeholder(limit_placeholder)
    )
}

fn build_batch_update_sql(
    table: &str,
    contract: &CursorContract,
    set_clause: &str,
    filter: Option<&str>,
    have_last: bool,
) -> String {
    let columns = contract
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect::<Vec<_>>();
    let (predicate, limit_placeholder) = window_predicate(&contract.columns, filter, have_last);
    let keys = key_projection(&columns);
    // Match the selected window through the exact candidate-key comparison
    // contract. A UNIQUE index may deliberately override the column's declared
    // collation (for example, a BINARY unique key on a NOCASE column), so bare
    // row-value membership could otherwise match more rows than paging selected.
    let comparison_keys = contract
        .columns
        .iter()
        .map(comparison_expression)
        .collect::<Vec<_>>()
        .join(", ");
    let lhs = if contract.columns.len() == 1 {
        comparison_keys.clone()
    } else {
        format!("({comparison_keys})")
    };
    format!(
        "UPDATE {} SET {set_clause} WHERE {lhs} IN (SELECT {comparison_keys} FROM {} WHERE {predicate} ORDER BY {} LIMIT {}) RETURNING {keys}",
        quote_ident(table),
        quote_ident(table),
        order_by(&contract.columns, false),
        sqlite_placeholder(limit_placeholder)
    )
}

fn build_end_cursor_sql(table: &str, contract: &CursorContract, filter: Option<&str>) -> String {
    let predicate = filter
        .map(|filter| format!(" WHERE ({filter})"))
        .unwrap_or_default();
    let columns = contract
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect::<Vec<_>>();
    format!(
        "SELECT {} FROM {}{predicate} ORDER BY {} LIMIT 1",
        cursor_projection(&columns),
        quote_ident(table),
        order_by(&contract.columns, true)
    )
}

fn row_to_tuple(
    row: &[Option<String>],
    live: &LiveCursorContract,
) -> Result<CursorTuple, SqliteActorError> {
    if row.len() != live.kinds.len() * 2 {
        return Err(SqliteActorError::Exec(format!(
            "cursor query returned {} cells; expected {}",
            row.len(),
            live.kinds.len() * 2
        )));
    }
    let mut values = Vec::with_capacity(live.kinds.len());
    for (index, kind) in live.kinds.iter().enumerate() {
        let value = row[index * 2].clone().ok_or_else(|| {
            SqliteActorError::Exec(format!("cursor tuple component {index} is null"))
        })?;
        let storage_class = row[index * 2 + 1].as_deref();
        if storage_class != Some(kind.storage_class()) {
            return Err(SqliteActorError::Exec(format!(
                "cursor tuple component {index} has storage class {storage_class:?}; expected {:?}",
                kind.storage_class()
            )));
        }
        values.push(match kind {
            SqliteCursorKind::Integer => IrScalar::Int64(value.parse::<i64>().map_err(|_| {
                SqliteActorError::Exec(format!(
                    "cursor tuple component {index} is not an exact signed 64-bit integer"
                ))
            })?),
            SqliteCursorKind::Text => IrScalar::Str(value),
        });
    }
    CursorTuple::new(values, &live.contract)
        .map_err(|error| SqliteActorError::Exec(error.to_string()))
}

fn tuple_binds(tuple: &CursorTuple) -> Result<Vec<SqliteBind>, SqliteActorError> {
    tuple
        .values()
        .iter()
        .map(|value| match value {
            IrScalar::Int(value) | IrScalar::Int64(value) => Ok(SqliteBind::Int(*value)),
            IrScalar::Decimal(value) | IrScalar::Str(value) => Ok(SqliteBind::Text(value.clone())),
            other => Err(SqliteActorError::Exec(format!(
                "unsupported cursor scalar at executor boundary: {other:?}"
            ))),
        })
        .collect()
}

fn window_binds(
    last: Option<&CursorTuple>,
    end: &CursorTuple,
    batch_size: u32,
) -> Result<Vec<SqliteBind>, SqliteActorError> {
    let mut binds = Vec::new();
    if let Some(last) = last {
        binds.extend(tuple_binds(last)?);
    }
    binds.extend(tuple_binds(end)?);
    binds.push(SqliteBind::Int(i64::from(batch_size)));
    Ok(binds)
}

fn guard_name(backfill_id: &str) -> String {
    let digest = Sha256::digest(backfill_id.as_bytes());
    format!("zero_migrate_cursor_guard_{}", hex::encode(&digest[..10]))
}

fn guard_sql(spec: &BackfillSpec, name: &str) -> String {
    let update_columns = spec
        .cursor_columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let changed = spec
        .cursor_columns
        .iter()
        .map(|column| {
            let quoted = quote_ident(column);
            // Compare the stored representation, not the cursor's paging
            // collation. `NOCASE`/`RTRIM` may consider two distinct text values
            // equal for ordering and uniqueness, but the stability contract
            // forbids changing a component at all while checkpoints are live.
            format!("OLD.{quoted} COLLATE BINARY IS NOT NEW.{quoted} COLLATE BINARY")
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(
        "CREATE TRIGGER {} BEFORE UPDATE OF {update_columns} ON {} FOR EACH ROW WHEN {changed} BEGIN SELECT RAISE(ABORT, 'zero-migrate cursor stability guard'); END",
        quote_ident(name),
        quote_ident(&spec.table)
    )
}

fn normalize_trigger_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

async fn target_triggers(
    actor: &MigrationActor,
    table: &str,
) -> Result<Vec<(String, String)>, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    let rows = actor
        .query_params(
            "SELECT name, sql FROM main.sqlite_schema WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name",
            &[SqliteBind::Text(table.to_string())],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            let name = row.first().and_then(Clone::clone).ok_or_else(|| {
                SqliteActorError::Exec("target trigger has a null name".to_string())
            })?;
            let sql = row.get(1).and_then(Clone::clone).ok_or_else(|| {
                SqliteActorError::Exec(format!("target trigger {name:?} has null SQL"))
            })?;
            Ok((name, sql))
        })
        .collect()
}

async fn ensure_trigger_state(
    actor: &MigrationActor,
    spec: &BackfillSpec,
    expected_guard: Option<&str>,
) -> Result<(), SqliteActorError> {
    let triggers = target_triggers(actor, &spec.table).await?;
    match expected_guard {
        None if triggers.is_empty() => Ok(()),
        None => Err(SqliteActorError::Exec(format!(
            "sqlite backfill target {:?} has existing trigger {:?}; its interaction cannot be proven safe",
            spec.table, triggers[0].0
        ))),
        Some(name) if triggers.len() == 1 && triggers[0].0 == name => {
            let expected = normalize_trigger_sql(&guard_sql(spec, name));
            let actual = normalize_trigger_sql(&triggers[0].1);
            if actual == expected {
                Ok(())
            } else {
                Err(SqliteActorError::Exec(format!(
                    "zero-migrate cursor guard {name:?} was replaced or changed"
                )))
            }
        }
        Some(name) => Err(SqliteActorError::Exec(format!(
            "expected the sole target trigger to be zero-migrate guard {name:?}; found {triggers:?}"
        ))),
    }
}

fn stability_parts(stability: &CursorStability) -> (&'static str, Option<&str>) {
    match stability {
        CursorStability::GuardUpdates => ("guardUpdates", None),
        CursorStability::ExternalInvariant { name } => ("externalInvariant", Some(name)),
    }
}

fn cohort_checksum(
    checksum: &str,
    target_table: &str,
    cursor_columns_json: &str,
    cursor_contract_json: &str,
    stability_mode: &str,
    stability_name: Option<&str>,
    end_cursor_json: Option<&str>,
) -> String {
    let mut digest = Sha256::new();
    for value in [
        checksum,
        target_table,
        cursor_columns_json,
        cursor_contract_json,
        stability_mode,
        stability_name.unwrap_or("\0<none>"),
        end_cursor_json.unwrap_or("\0<empty>"),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn decode_progress_bool(
    row: &[Option<String>],
    index: usize,
    column: &str,
) -> Result<bool, SqliteActorError> {
    match row.get(index).and_then(|cell| cell.as_deref()) {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        actual => Err(SqliteActorError::Exec(format!(
            "backfill progress has invalid {column} value {actual:?}"
        ))),
    }
}

fn sqlite_journal_err(error: SqliteActorError) -> BackfillError {
    BackfillError::Journal(crate::apply::journal::JournalError::Backend(
        error.to_string(),
    ))
}

fn batch_error(last: Option<&CursorTuple>, error: SqliteActorError) -> BackfillError {
    match error {
        SqliteActorError::Poisoned(message) => BackfillError::SqlitePoisoned(message),
        error => BackfillError::SqliteBatchFailed {
            at_cursor: last.and_then(|cursor| cursor.to_json().ok()),
            source_msg: error.to_string(),
        },
    }
}

/// Create the current pre-release progress schema directly. Old development
/// schemas are rejected rather than normalized through a compatibility spelling.
pub(crate) async fn ensure_backfill_progress(
    actor: &MigrationActor,
) -> Result<(), SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    actor
        .exec(
            "CREATE TABLE IF NOT EXISTS \"_mig\".schema_backfills (\
                backfill_id TEXT PRIMARY KEY, \
                checksum TEXT NOT NULL, \
                name TEXT NOT NULL, \
                target_table TEXT NOT NULL, \
                cursor_columns TEXT NOT NULL, \
                cursor_contract TEXT NOT NULL, \
                stability_mode TEXT NOT NULL, \
                stability_name TEXT, \
                guard_name TEXT, \
                guard_installed INTEGER NOT NULL, \
                guard_cleaned INTEGER NOT NULL, \
                last_cursor TEXT, \
                end_cursor TEXT, \
                cohort_checksum TEXT NOT NULL, \
                cohort_initialized INTEGER NOT NULL, \
                rows_done INTEGER NOT NULL DEFAULT 0, \
                batches_done INTEGER NOT NULL DEFAULT 0, \
                complete INTEGER NOT NULL DEFAULT 0, \
                applied_by TEXT NOT NULL, \
                started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
        )
        .await?;
    let columns = actor
        .query("PRAGMA \"_mig\".table_info(schema_backfills)")
        .await?;
    let actual = columns
        .iter()
        .filter_map(|row| row.get(1).and_then(Clone::clone))
        .collect::<Vec<_>>();
    if actual.iter().map(String::as_str).collect::<Vec<_>>() != PROGRESS_COLUMNS {
        return Err(SqliteActorError::Exec(format!(
            "schema_backfills uses a stale pre-release schema {actual:?}; recreate the development migration database (expected {PROGRESS_COLUMNS:?})"
        )));
    }
    Ok(())
}

pub(crate) async fn read_progress_entries(
    actor: &MigrationActor,
) -> Result<Vec<BackfillProgressEntry>, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    let exists = actor
        .query(
            "SELECT 1 FROM \"_mig\".sqlite_schema WHERE type = 'table' AND name = 'schema_backfills' LIMIT 1",
        )
        .await?;
    if exists.is_empty() {
        return Ok(Vec::new());
    }
    ensure_backfill_progress(actor).await?;
    let rows = actor
        .query(
            "SELECT backfill_id, checksum, complete FROM \"_mig\".schema_backfills ORDER BY backfill_id",
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            let version = row.first().and_then(Clone::clone).ok_or_else(|| {
                SqliteActorError::Exec("backfill progress row has null identity".to_string())
            })?;
            let checksum = row.get(1).and_then(Clone::clone).ok_or_else(|| {
                SqliteActorError::Exec(format!(
                    "backfill progress row {version:?} has null checksum"
                ))
            })?;
            Ok(BackfillProgressEntry {
                version,
                checksum: Some(checksum),
                complete: decode_progress_bool(&row, 2, "complete")?,
            })
        })
        .collect()
}

fn absent_progress() -> Progress {
    Progress {
        checksum: String::new(),
        target_table: String::new(),
        cursor_columns_json: String::new(),
        cursor_contract_json: String::new(),
        stability_mode: String::new(),
        stability_name: None,
        guard_name: None,
        guard_installed: false,
        guard_cleaned: false,
        last_cursor_json: None,
        end_cursor_json: None,
        cohort_checksum: String::new(),
        cohort_initialized: false,
        complete: false,
        exists: false,
    }
}

async fn read_progress(
    actor: &MigrationActor,
    backfill_id: &str,
) -> Result<Progress, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    let rows = actor
        .query_params(
            "SELECT checksum, target_table, cursor_columns, cursor_contract, \
                    stability_mode, stability_name, guard_name, guard_installed, \
                    guard_cleaned, last_cursor, end_cursor, cohort_checksum, \
                    cohort_initialized, complete \
               FROM \"_mig\".schema_backfills WHERE backfill_id = ?1",
            &[SqliteBind::Text(backfill_id.to_string())],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(absent_progress());
    };
    if rows.len() != 1 {
        return Err(SqliteActorError::Exec(format!(
            "backfill progress lookup returned {} rows for {backfill_id:?}",
            rows.len()
        )));
    }
    Ok(Progress {
        checksum: required_progress_cell(row, 0, "checksum")?,
        target_table: required_progress_cell(row, 1, "target_table")?,
        cursor_columns_json: required_progress_cell(row, 2, "cursor_columns")?,
        cursor_contract_json: required_progress_cell(row, 3, "cursor_contract")?,
        stability_mode: required_progress_cell(row, 4, "stability_mode")?,
        stability_name: row.get(5).and_then(Clone::clone),
        guard_name: row.get(6).and_then(Clone::clone),
        guard_installed: decode_progress_bool(row, 7, "guard_installed")?,
        guard_cleaned: decode_progress_bool(row, 8, "guard_cleaned")?,
        last_cursor_json: row.get(9).and_then(Clone::clone),
        end_cursor_json: row.get(10).and_then(Clone::clone),
        cohort_checksum: required_progress_cell(row, 11, "cohort_checksum")?,
        cohort_initialized: decode_progress_bool(row, 12, "cohort_initialized")?,
        complete: decode_progress_bool(row, 13, "complete")?,
        exists: true,
    })
}

fn required_progress_cell(
    row: &[Option<String>],
    index: usize,
    column: &str,
) -> Result<String, SqliteActorError> {
    row.get(index).and_then(Clone::clone).ok_or_else(|| {
        SqliteActorError::Exec(format!(
            "backfill progress row has null required column {column}"
        ))
    })
}

fn validate_progress(
    progress: &Progress,
    checksum: &str,
    spec: &BackfillSpec,
    live: &LiveCursorContract,
) -> Result<(Option<CursorTuple>, Option<CursorTuple>), SqliteActorError> {
    if !progress.exists {
        return Err(SqliteActorError::Exec(
            "backfill progress row is absent".to_string(),
        ));
    }
    let columns_json = serde_json::to_string(&spec.cursor_columns)
        .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
    let contract_json = serde_json::to_string(&live.contract)
        .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
    let (stability_mode, stability_name) = stability_parts(&spec.cursor_stability);
    if progress.checksum != checksum
        || progress.target_table != spec.table
        || progress.cursor_columns_json != columns_json
        || progress.cursor_contract_json != contract_json
        || progress.stability_mode != stability_mode
        || progress.stability_name.as_deref() != stability_name
    {
        return Err(SqliteActorError::Exec(
            "backfill progress drift: checksum/target/cursor columns/scalar types/collation semantics no longer match the plan"
                .to_string(),
        ));
    }
    let expected_cohort_checksum = cohort_checksum(
        checksum,
        &spec.table,
        &columns_json,
        &contract_json,
        stability_mode,
        stability_name,
        progress.end_cursor_json.as_deref(),
    );
    if progress.cohort_checksum != expected_cohort_checksum {
        return Err(SqliteActorError::Exec(
            "backfill cohort bound changed after initialization".to_string(),
        ));
    }
    if !progress.cohort_initialized {
        return Err(SqliteActorError::Exec(
            "backfill cohort was not durably initialized".to_string(),
        ));
    }
    let last = progress
        .last_cursor_json
        .as_deref()
        .map(|json| CursorTuple::from_json(json, &live.contract))
        .transpose()
        .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
    let end = progress
        .end_cursor_json
        .as_deref()
        .map(|json| CursorTuple::from_json(json, &live.contract))
        .transpose()
        .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
    if last.is_some() && end.is_none() {
        return Err(SqliteActorError::Exec(
            "backfill progress has a checkpoint for an empty cohort".to_string(),
        ));
    }
    match &spec.cursor_stability {
        CursorStability::GuardUpdates => {
            let expected = guard_name_for_progress(progress)?;
            if !progress.guard_installed {
                return Err(SqliteActorError::Exec(format!(
                    "cursor guard {expected:?} was never durably installed"
                )));
            }
            if progress.complete != progress.guard_cleaned {
                return Err(SqliteActorError::Exec(
                    "cursor guard cleanup obligation disagrees with completion".to_string(),
                ));
            }
        }
        CursorStability::ExternalInvariant { .. } => {
            if progress.guard_name.is_some() || progress.guard_installed || progress.guard_cleaned {
                return Err(SqliteActorError::Exec(
                    "external-invariant progress unexpectedly records a database guard".to_string(),
                ));
            }
        }
    }
    Ok((last, end))
}

fn guard_name_for_progress(progress: &Progress) -> Result<&str, SqliteActorError> {
    progress.guard_name.as_deref().ok_or_else(|| {
        SqliteActorError::Exec("guardUpdates progress has no guard name".to_string())
    })
}

#[allow(clippy::too_many_arguments)]
async fn initialize_progress(
    actor: &MigrationActor,
    backfill_id: &str,
    checksum: &str,
    spec: &BackfillSpec,
    applied_by: &str,
) -> Result<(LiveCursorContract, Option<CursorTuple>), SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;
    let result = async {
        let live = resolve_live_cursor_contract(actor, spec)
            .await
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        validate_storage_classes(actor, spec, &live)
            .await
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        ensure_trigger_state(actor, spec, None).await?;

        let installed_guard = match &spec.cursor_stability {
            CursorStability::GuardUpdates => {
                let name = guard_name(backfill_id);
                actor.set_mode(Mode::EngineJournal).await?;
                actor.exec(&guard_sql(spec, &name)).await?;
                ensure_trigger_state(actor, spec, Some(&name)).await?;
                Some(name)
            }
            CursorStability::ExternalInvariant { .. } => None,
        };

        actor.set_mode(Mode::CreatorUp).await?;
        let rows = actor
            .query(&build_end_cursor_sql(
                &spec.table,
                &live.contract,
                spec.filter.as_deref(),
            ))
            .await?;
        let end = rows
            .first()
            .map(|row| row_to_tuple(row, &live))
            .transpose()?;
        let end_json = end
            .as_ref()
            .map(CursorTuple::to_json)
            .transpose()
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        let columns_json = serde_json::to_string(&spec.cursor_columns)
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        let contract_json = serde_json::to_string(&live.contract)
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        let (stability_mode, stability_name) = stability_parts(&spec.cursor_stability);
        let cohort = cohort_checksum(
            checksum,
            &spec.table,
            &columns_json,
            &contract_json,
            stability_mode,
            stability_name,
            end_json.as_deref(),
        );
        actor.set_mode(Mode::EngineJournal).await?;
        actor
            .exec_params(
                "INSERT INTO \"_mig\".schema_backfills \
                    (backfill_id, checksum, name, target_table, cursor_columns, \
                     cursor_contract, stability_mode, stability_name, guard_name, \
                     guard_installed, guard_cleaned, end_cursor, cohort_checksum, \
                     cohort_initialized, applied_by) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11, ?12, 1, ?13)",
                &[
                    SqliteBind::Text(backfill_id.to_string()),
                    SqliteBind::Text(checksum.to_string()),
                    SqliteBind::Text(spec.name.clone()),
                    SqliteBind::Text(spec.table.clone()),
                    SqliteBind::Text(columns_json),
                    SqliteBind::Text(contract_json),
                    SqliteBind::Text(stability_mode.to_string()),
                    stability_name
                        .map_or(SqliteBind::Null, |name| SqliteBind::Text(name.to_string())),
                    installed_guard
                        .as_ref()
                        .map_or(SqliteBind::Null, |name| SqliteBind::Text(name.clone())),
                    SqliteBind::Int(i64::from(installed_guard.is_some())),
                    end_json.map_or(SqliteBind::Null, SqliteBind::Text),
                    SqliteBind::Text(cohort),
                    SqliteBind::Text(applied_by.to_string()),
                ],
            )
            .await?;
        Ok::<_, SqliteActorError>((live, end))
    }
    .await;
    match result {
        Ok(value) => {
            actor
                .commit_or_cleanup("backfill cohort initialization")
                .await?;
            Ok(value)
        }
        Err(error) => Err(actor
            .cleanup_after_error("backfill cohort initialization", error)
            .await),
    }
}

fn build_per_row_update_sql(
    spec: &BackfillSpec,
    set_clause: &str,
    contract: &CursorContract,
) -> String {
    let mut assignments = Vec::new();
    if !set_clause.trim().is_empty() {
        assignments.push(set_clause.to_string());
    }
    for (index, column) in spec.per_row.keys().enumerate() {
        assignments.push(format!(
            "{} = {}",
            quote_ident(column),
            sqlite_placeholder(index + 1)
        ));
    }
    let cursor_first = spec.per_row.len() + 1;
    let predicate = contract
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            format!(
                "{} = {}",
                comparison_expression(column),
                sqlite_placeholder(cursor_first + index)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    format!(
        "UPDATE {} SET {} WHERE {predicate} RETURNING {}",
        quote_ident(&spec.table),
        assignments.join(", "),
        key_projection(&spec.cursor_columns)
    )
}

#[allow(clippy::too_many_arguments)]
async fn run_batch(
    actor: &MigrationActor,
    backfill_id: &str,
    checksum: &str,
    spec: &BackfillSpec,
    set_clause: &str,
    expected_last: Option<&CursorTuple>,
    expected_end: &CursorTuple,
) -> Result<(u64, Option<CursorTuple>), BackfillError> {
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(|error| batch_error(expected_last, error))?;
    actor
        .exec("BEGIN IMMEDIATE")
        .await
        .map_err(|error| batch_error(expected_last, error))?;
    let result = async {
        let live = resolve_live_cursor_contract(actor, spec)
            .await
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        validate_storage_classes(actor, spec, &live)
            .await
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        let progress = read_progress(actor, backfill_id).await?;
        let (actual_last, actual_end) = validate_progress(&progress, checksum, spec, &live)?;
        if actual_last.as_ref() != expected_last || actual_end.as_ref() != Some(expected_end) {
            return Err(SqliteActorError::Exec(
                "backfill checkpoint or cohort bound changed before the batch".to_string(),
            ));
        }
        let expected_guard = match &spec.cursor_stability {
            CursorStability::GuardUpdates => Some(guard_name_for_progress(&progress)?),
            CursorStability::ExternalInvariant { .. } => None,
        };
        ensure_trigger_state(actor, spec, expected_guard).await?;

        let binds = window_binds(expected_last, expected_end, spec.batch_size)?;
        let window_sql = build_window_sql(
            &spec.table,
            &live.contract,
            spec.filter.as_deref(),
            expected_last.is_some(),
        );
        actor.set_mode(Mode::CreatorUp).await?;
        let selected_rows = actor.query_params(&window_sql, &binds).await?;
        let selected = selected_rows
            .iter()
            .map(|row| row_to_tuple(row, &live))
            .collect::<Result<Vec<_>, _>>()?;
        if selected.is_empty() {
            return Ok((0, None));
        }
        let new_last = selected.last().cloned().ok_or_else(|| {
            SqliteActorError::Exec("non-empty cursor window has no tail".to_string())
        })?;

        if spec.per_row.is_empty() {
            let sql = build_batch_update_sql(
                &spec.table,
                &live.contract,
                set_clause,
                spec.filter.as_deref(),
                expected_last.is_some(),
            );
            let returned = actor.query_params(&sql, &binds).await?;
            if returned.len() != selected.len() {
                return Err(SqliteActorError::Exec(format!(
                    "backfill selected {} rows but updated {}; a trigger, policy, or conflict suppressed the window",
                    selected.len(),
                    returned.len()
                )));
            }
        } else {
            let sql = build_per_row_update_sql(spec, set_clause, &live.contract);
            for cursor in &selected {
                let mut row_binds = spec
                    .per_row
                    .values()
                    .map(|assignment| {
                        SqliteBind::Text(generate_per_row_value(assignment.generator()))
                    })
                    .collect::<Vec<_>>();
                row_binds.extend(tuple_binds(cursor)?);
                let returned = actor.query_params(&sql, &row_binds).await?;
                if returned.len() != 1 {
                    return Err(SqliteActorError::Exec(format!(
                        "per-row backfill at cursor {:?} affected {} rows",
                        cursor.values(),
                        returned.len()
                    )));
                }
            }
        }

        let old_last_json = expected_last
            .map(CursorTuple::to_json)
            .transpose()
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        let new_last_json = new_last
            .to_json()
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        actor.set_mode(Mode::EngineJournal).await?;
        let advanced = actor
            .query_params(
                "UPDATE \"_mig\".schema_backfills \
                    SET last_cursor = ?1, rows_done = rows_done + ?2, \
                        batches_done = batches_done + 1 \
                  WHERE backfill_id = ?3 AND checksum = ?4 AND last_cursor IS ?5 \
                    AND end_cursor = ?6 AND cohort_checksum = ?7 \
                    AND cohort_initialized = 1 AND complete = 0 \
                RETURNING backfill_id",
                &[
                    SqliteBind::Text(new_last_json),
                    SqliteBind::Int(i64::try_from(selected.len()).map_err(|_| {
                        SqliteActorError::Exec("backfill batch row count exceeds i64".to_string())
                    })?),
                    SqliteBind::Text(backfill_id.to_string()),
                    SqliteBind::Text(checksum.to_string()),
                    old_last_json.map_or(SqliteBind::Null, SqliteBind::Text),
                    SqliteBind::Text(expected_end.to_json().map_err(|error| {
                        SqliteActorError::Exec(error.to_string())
                    })?),
                    SqliteBind::Text(progress.cohort_checksum),
                ],
            )
            .await?;
        if advanced.len() != 1 {
            return Err(SqliteActorError::Exec(format!(
                "backfill checkpoint advance affected {} rows; expected one",
                advanced.len()
            )));
        }
        Ok((selected.len() as u64, Some(new_last)))
    }
    .await;
    match result {
        Ok(value) => match actor.commit_or_cleanup("backfill batch").await {
            Ok(()) => Ok(value),
            Err(error) => Err(batch_error(expected_last, error)),
        },
        Err(error) => Err(batch_error(
            expected_last,
            actor.cleanup_after_error("backfill batch", error).await,
        )),
    }
}

async fn complete_progress(
    actor: &MigrationActor,
    backfill_id: &str,
    checksum: &str,
    spec: &BackfillSpec,
    expected_last: Option<&CursorTuple>,
    identity: Option<PlanBackfillIdentity<'_>>,
    applied_by: &str,
) -> Result<(), SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;
    let result = async {
        let live = resolve_live_cursor_contract(actor, spec)
            .await
            .map_err(|error| SqliteActorError::Exec(error.to_string()))?;
        let progress = read_progress(actor, backfill_id).await?;
        let (actual_last, _) = validate_progress(&progress, checksum, spec, &live)?;
        if actual_last.as_ref() != expected_last {
            return Err(SqliteActorError::Exec(
                "backfill checkpoint changed before completion".to_string(),
            ));
        }
        match &spec.cursor_stability {
            CursorStability::GuardUpdates => {
                let guard = guard_name_for_progress(&progress)?;
                if !progress.complete {
                    ensure_trigger_state(actor, spec, Some(guard)).await?;
                    actor.set_mode(Mode::EngineJournal).await?;
                    actor
                        .exec(&format!("DROP TRIGGER {}", quote_ident(guard)))
                        .await?;
                }
                ensure_trigger_state(actor, spec, None).await?;
            }
            CursorStability::ExternalInvariant { .. } => {
                ensure_trigger_state(actor, spec, None).await?;
            }
        }
        actor.set_mode(Mode::EngineJournal).await?;
        let completed = actor
            .query_params(
                "UPDATE \"_mig\".schema_backfills \
                    SET complete = 1, guard_cleaned = CASE WHEN guard_installed = 1 THEN 1 ELSE 0 END \
                  WHERE backfill_id = ?1 AND checksum = ?2 AND cohort_checksum = ?3 \
                    AND complete IN (0, 1) \
                RETURNING backfill_id",
                &[
                    SqliteBind::Text(backfill_id.to_string()),
                    SqliteBind::Text(checksum.to_string()),
                    SqliteBind::Text(progress.cohort_checksum),
                ],
            )
            .await?;
        if completed.len() != 1 {
            return Err(SqliteActorError::Exec(format!(
                "backfill completion affected {} progress rows; expected one",
                completed.len()
            )));
        }

        if let Some(identity) = identity {
            let latest = actor
                .query(&format!(
                    "SELECT event_kind, checksum FROM \"_mig\".schema_migrations \
                      WHERE version = {} ORDER BY event_seq DESC LIMIT 1",
                    sql_lit(identity.version.as_str())
                ))
                .await?;
            let already_matching = latest.first().is_some_and(|row| {
                row.first().and_then(|cell| cell.as_deref()) == Some("applied")
                    && row.get(1).and_then(|cell| cell.as_deref())
                        == Some(identity.checksum.as_str())
            });
            if latest.first().is_some_and(|row| {
                row.first().and_then(|cell| cell.as_deref()) == Some("applied")
                    && row.get(1).and_then(|cell| cell.as_deref())
                        != Some(identity.checksum.as_str())
            }) {
                return Err(SqliteActorError::Exec(format!(
                    "checksum drift while finalizing backfill {}",
                    identity.version.as_str()
                )));
            }
            if !already_matching {
                actor
                    .exec(&format!(
                        "INSERT INTO \"_mig\".schema_migrations \
                            (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
                         VALUES ('applied', {}, {}, {}, {}, 'completed', 'success', 'apply')",
                        sql_lit(identity.version.as_str()),
                        sql_lit(&spec.name),
                        sql_lit(identity.checksum.as_str()),
                        sql_lit(applied_by),
                    ))
                    .await?;
            }
        }
        Ok::<(), SqliteActorError>(())
    }
    .await;
    match result {
        Ok(()) => actor.commit_or_cleanup("backfill finalization").await,
        Err(error) => Err(actor
            .cleanup_after_error("backfill finalization", error)
            .await),
    }
}

/// Run or resume an ordered, bounded SQLite backfill.
pub(crate) async fn run_backfill_bounded(
    actor: &MigrationActor,
    spec: &BackfillSpec,
    set_clause: &str,
    filter: Option<&str>,
    applied_by: &str,
    max_batches: Option<u64>,
    identity: Option<PlanBackfillIdentity<'_>>,
) -> Result<BackfillOutcome, BackfillError> {
    validate_spec(spec, set_clause)?;
    if filter != spec.filter.as_deref() {
        return Err(BackfillError::InvalidSpec(
            "executor filter differs from the planned backfill filter".to_string(),
        ));
    }
    ensure_backfill_progress(actor)
        .await
        .map_err(sqlite_journal_err)?;

    let backfill_id = identity.map_or_else(
        || spec.backfill_id(),
        |identity| identity.version.as_str().to_string(),
    );
    // Direct test/backend calls have no migration envelope. Their stable spec id
    // serves as the checksum anchor; planned calls always use the plan checksum.
    let expected_checksum = identity.map_or_else(
        || spec.backfill_id(),
        |identity| identity.checksum.as_str().to_string(),
    );
    let live = resolve_live_cursor_contract(actor, spec).await?;
    validate_storage_classes(actor, spec, &live).await?;
    let mut progress = read_progress(actor, &backfill_id)
        .await
        .map_err(sqlite_journal_err)?;
    let existed = progress.exists;
    let (mut last, end) = if progress.exists {
        let tuple = validate_progress(&progress, &expected_checksum, spec, &live)
            .map_err(sqlite_journal_err)?;
        let expected_guard = match &spec.cursor_stability {
            CursorStability::GuardUpdates if !progress.complete => {
                Some(guard_name_for_progress(&progress).map_err(sqlite_journal_err)?)
            }
            _ => None,
        };
        ensure_trigger_state(actor, spec, expected_guard)
            .await
            .map_err(sqlite_journal_err)?;
        tuple
    } else {
        let (initialized_live, initialized_end) =
            initialize_progress(actor, &backfill_id, &expected_checksum, spec, applied_by)
                .await
                .map_err(sqlite_journal_err)?;
        if initialized_live.contract != live.contract {
            return Err(cursor_unavailable(
                spec,
                "cursor contract changed while cohort initialization acquired its lock",
            ));
        }
        progress = read_progress(actor, &backfill_id)
            .await
            .map_err(sqlite_journal_err)?;
        (None, initialized_end)
    };
    let resumed = existed;
    if progress.complete {
        if let Some(identity) = identity {
            complete_progress(
                actor,
                &backfill_id,
                &expected_checksum,
                spec,
                last.as_ref(),
                Some(identity),
                applied_by,
            )
            .await
            .map_err(sqlite_journal_err)?;
        }
        return Ok(BackfillOutcome {
            backfill_id,
            batches: 0,
            rows_updated: 0,
            resumed,
            complete: true,
        });
    }

    let mut batches = 0_u64;
    let mut rows_updated = 0_u64;
    let mut tail = end.is_none();
    if let Some(end) = end.as_ref() {
        loop {
            if max_batches.is_some_and(|limit| batches >= limit) {
                break;
            }
            let (count, next) = run_batch(
                actor,
                &backfill_id,
                &expected_checksum,
                spec,
                set_clause,
                last.as_ref(),
                end,
            )
            .await?;
            if count == 0 {
                tail = true;
                break;
            }
            batches += 1;
            rows_updated += count;
            last = next;
            if let Err(error) = crate::fault::trip(crate::fault::points::BACKFILL_MID_BATCHES) {
                return Err(BackfillError::Fault(error.to_string()));
            }
            if count < u64::from(spec.batch_size) {
                tail = true;
                break;
            }
        }
    }
    if tail {
        complete_progress(
            actor,
            &backfill_id,
            &expected_checksum,
            spec,
            last.as_ref(),
            identity,
            applied_by,
        )
        .await
        .map_err(sqlite_journal_err)?;
    }
    Ok(BackfillOutcome {
        backfill_id,
        batches,
        rows_updated,
        resumed,
        complete: tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract_column(
        name: &str,
        scalar_type: CursorScalarType,
        database_type: &str,
        comparison: CursorComparison,
    ) -> CursorColumnContract {
        CursorColumnContract {
            name: name.to_string(),
            scalar_type,
            database_type: database_type.to_string(),
            comparison,
        }
    }

    fn external_spec(table: &str, columns: &[&str], batch_size: u32) -> BackfillSpec {
        BackfillSpec {
            schema: "main".to_string(),
            table: table.to_string(),
            cursor_columns: columns.iter().map(|column| (*column).to_string()).collect(),
            cursor_stability: CursorStability::ExternalInvariant {
                name: format!("{table}_cursor_is_immutable"),
            },
            cursor_contract: None,
            batch_size,
            set_clause: "\"done\" = 1".to_string(),
            per_row: BTreeMap::new(),
            filter: Some("\"done\" = 0".to_string()),
            name: format!("fill_{table}"),
        }
    }

    fn open_actor(tag: &str) -> (tempfile::TempDir, MigrationActor) {
        let directory = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &directory.path().join(format!("{tag}.sqlite")),
            &directory.path().join(format!("{tag}.journal.sqlite")),
        )
        .expect("open sqlite migration actor");
        (directory, actor)
    }

    #[test]
    fn lexicographic_disjunction_preserves_declared_order_and_boundaries() {
        let columns = vec![
            contract_column(
                "tenant",
                CursorScalarType::String,
                "text",
                CursorComparison::CaseInsensitive,
            ),
            contract_column(
                "sequence",
                CursorScalarType::Int64,
                "integer",
                CursorComparison::Default,
            ),
        ];
        assert_eq!(
            lexicographic_after(&columns, 1),
            "((\"tenant\" COLLATE \"NOCASE\" > ?1) OR (\"tenant\" COLLATE \"NOCASE\" = ?1 AND \"sequence\" COLLATE \"BINARY\" > ?2))"
        );
        assert_eq!(
            lexicographic_at_or_before(&columns, 3),
            "((\"tenant\" COLLATE \"NOCASE\" < ?3) OR (\"tenant\" COLLATE \"NOCASE\" = ?3 AND \"sequence\" COLLATE \"BINARY\" <= ?4))"
        );
        let (predicate, limit) = window_predicate(&columns, Some("\"done\" = 0"), true);
        assert!(predicate.contains("\"tenant\" COLLATE \"NOCASE\" > ?1"));
        assert!(predicate.contains("\"tenant\" COLLATE \"NOCASE\" < ?3"));
        assert!(predicate.ends_with("AND (\"done\" = 0)"));
        assert_eq!(limit, 5);
    }

    #[test]
    fn update_matching_uses_the_proven_candidate_key_collation() {
        let contract = CursorContract {
            columns: vec![contract_column(
                "cursor_value",
                CursorScalarType::String,
                "text",
                CursorComparison::Default,
            )],
        };
        let batch = build_batch_update_sql(
            "items",
            &contract,
            "\"done\" = 1",
            Some("\"done\" = 0"),
            false,
        );
        assert!(batch.contains(
            "WHERE \"cursor_value\" COLLATE \"BINARY\" IN (SELECT \"cursor_value\" COLLATE \"BINARY\""
        ));

        let mut spec = external_spec("items", &["cursor_value"], 1);
        spec.per_row.insert(
            "generated".to_string(),
            crate::model::backfill::PerRowAssignment::validated(
                "main",
                "items",
                "generated",
                PerRowGenerator::UuidV4,
            ),
        );
        let per_row = build_per_row_update_sql(&spec, &spec.set_clause, &contract);
        assert!(per_row.contains("WHERE \"cursor_value\" COLLATE \"BINARY\" = ?2"));
    }

    #[test]
    fn mutation_scan_rejects_every_cursor_component_without_literal_false_positives() {
        assert!(matches!(
            assert_component_not_mutated("\"a\" = 1, \"b\" = 2", "b"),
            Err(BackfillError::CursorComponentMutated { cursor_component }) if cursor_component == "b"
        ));
        assert_component_not_mutated("\"value\" = ', \"b\" = 2'", "b")
            .expect("assignment-shaped string content is data");
    }

    #[test]
    fn tuple_checkpoint_is_a_tagged_scalar_array_not_a_joined_string() {
        let contract = CursorContract {
            columns: vec![
                CursorColumnContract {
                    name: "tenant".to_string(),
                    scalar_type: CursorScalarType::String,
                    database_type: "TEXT".to_string(),
                    comparison: CursorComparison::Default,
                },
                CursorColumnContract {
                    name: "sequence".to_string(),
                    scalar_type: CursorScalarType::Int64,
                    database_type: "INTEGER".to_string(),
                    comparison: CursorComparison::Default,
                },
            ],
        };
        let tuple = CursorTuple::new(
            vec![
                IrScalar::Str("contains|delimiter\0safely".to_string()),
                IrScalar::Int64(i64::MAX),
            ],
            &contract,
        )
        .expect("tuple");
        let json = tuple.to_json().expect("json");
        assert_eq!(
            json,
            r#"["contains|delimiter\u0000safely",{"int64":"9223372036854775807"}]"#
        );
        assert_eq!(CursorTuple::from_json(&json, &contract).unwrap(), tuple);
    }

    #[compio::test]
    async fn live_contract_canonicalizes_supported_unmanaged_sqlite_aliases() {
        let (_directory, actor) = open_actor("cursor_aliases");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE alias_items (
                    id BIGINT NOT NULL,
                    sequence UNSIGNED BIG INT NOT NULL,
                    code VARCHAR(191) NOT NULL,
                    done INTEGER NOT NULL DEFAULT 0,
                    UNIQUE (id, sequence, code)
                )",
            )
            .await
            .unwrap();
        let mut spec = external_spec("alias_items", &["id", "sequence", "code"], 10);
        let expected = CursorContract {
            columns: vec![
                contract_column(
                    "id",
                    CursorScalarType::Int64,
                    "integer",
                    CursorComparison::Default,
                ),
                contract_column(
                    "sequence",
                    CursorScalarType::Int64,
                    "integer",
                    CursorComparison::Default,
                ),
                contract_column(
                    "code",
                    CursorScalarType::String,
                    "text",
                    CursorComparison::Default,
                ),
            ],
        };
        spec.cursor_contract = Some(expected.clone());

        let live = resolve_live_cursor_contract(&actor, &spec)
            .await
            .expect("supported aliases share semantic cursor types");
        assert_eq!(live.contract, expected);
    }

    #[compio::test]
    async fn single_cursor_progress_is_typed_bounded_and_atomic() {
        let (_directory, actor) = open_actor("single");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, done INTEGER NOT NULL DEFAULT 0); \
                 INSERT INTO items (id) VALUES (1), (2), (3), (4), (5)",
            )
            .await
            .unwrap();
        let mut spec = external_spec("items", &["id"], 2);
        spec.cursor_contract = Some(CursorContract {
            columns: vec![contract_column(
                "id",
                CursorScalarType::Int64,
                "integer",
                CursorComparison::Default,
            )],
        });

        let first = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            Some(1),
            None,
        )
        .await
        .unwrap();
        assert_eq!(first.rows_updated, 2);
        assert!(!first.complete);

        actor.set_mode(Mode::EngineJournal).await.unwrap();
        let progress = actor
            .query(
                "SELECT cursor_columns, last_cursor, end_cursor, rows_done, \
                        batches_done, complete, stability_mode, stability_name, guard_name \
                   FROM \"_mig\".schema_backfills",
            )
            .await
            .unwrap();
        assert_eq!(progress[0][0].as_deref(), Some(r#"["id"]"#));
        assert_eq!(progress[0][1].as_deref(), Some(r#"[{"int64":"2"}]"#));
        assert_eq!(progress[0][2].as_deref(), Some(r#"[{"int64":"5"}]"#));
        assert_eq!(progress[0][3].as_deref(), Some("2"));
        assert_eq!(progress[0][4].as_deref(), Some("1"));
        assert_eq!(progress[0][5].as_deref(), Some("0"));
        assert_eq!(progress[0][6].as_deref(), Some("externalInvariant"));
        assert_eq!(progress[0][7].as_deref(), Some("items_cursor_is_immutable"));
        assert_eq!(progress[0][8], None);
        let columns = actor
            .query("PRAGMA \"_mig\".table_info(schema_backfills)")
            .await
            .unwrap();
        let names = columns
            .iter()
            .filter_map(|row| row.get(1).and_then(Clone::clone))
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == "cursor_columns"));
        assert!(!names.iter().any(|name| name == "cursor_column"));
        assert!(names.iter().any(|name| name == "stability_mode"));

        let resumed = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(resumed.resumed);
        assert!(resumed.complete);
        assert_eq!(resumed.rows_updated, 3);
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        let rows = actor
            .query("SELECT count(*) FROM items WHERE done = 1")
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("5"));
    }

    #[compio::test]
    async fn composite_primary_key_pages_lexicographically_with_tagged_arrays() {
        let (_directory, actor) = open_actor("composite");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE pairs (\
                    tenant TEXT COLLATE NOCASE NOT NULL, \
                    sequence INTEGER NOT NULL, \
                    done INTEGER NOT NULL DEFAULT 0, \
                    PRIMARY KEY (tenant, sequence)); \
                 INSERT INTO pairs (tenant, sequence) VALUES \
                    ('a', 1), ('a', 3), ('B', 1), ('b', 2), ('C', 1)",
            )
            .await
            .unwrap();
        let spec = external_spec("pairs", &["tenant", "sequence"], 2);
        let first = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            Some(1),
            None,
        )
        .await
        .unwrap();
        assert_eq!(first.rows_updated, 2);
        actor.set_mode(Mode::EngineJournal).await.unwrap();
        let progress = actor
            .query("SELECT last_cursor, end_cursor FROM \"_mig\".schema_backfills")
            .await
            .unwrap();
        assert_eq!(progress[0][0].as_deref(), Some(r#"["a",{"int64":"3"}]"#));
        assert_eq!(progress[0][1].as_deref(), Some(r#"["C",{"int64":"1"}]"#));

        let resumed = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(resumed.complete);
        assert_eq!(resumed.rows_updated, 3);
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        let rows = actor
            .query("SELECT count(*) FROM pairs WHERE done = 1")
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("5"));
    }

    #[compio::test]
    async fn unique_index_collation_controls_one_row_batch_update_matching() {
        let (_directory, actor) = open_actor("candidate_collation");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE collation_keys (\
                    cursor_value TEXT COLLATE NOCASE NOT NULL, \
                    done INTEGER NOT NULL DEFAULT 0); \
                 CREATE UNIQUE INDEX collation_keys_binary \
                    ON collation_keys (cursor_value COLLATE BINARY); \
                 INSERT INTO collation_keys (cursor_value) VALUES ('A'), ('a')",
            )
            .await
            .unwrap();
        let spec = external_spec("collation_keys", &["cursor_value"], 1);

        let first = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            Some(1),
            None,
        )
        .await
        .expect("the BINARY candidate key must update only its selected row");
        assert_eq!(first.rows_updated, 1);
        assert!(!first.complete);

        actor.set_mode(Mode::CreatorUp).await.unwrap();
        let first_batch = actor
            .query(
                "SELECT cursor_value, done FROM collation_keys \
                 ORDER BY cursor_value COLLATE BINARY",
            )
            .await
            .unwrap();
        assert_eq!(
            first_batch,
            vec![
                vec![Some("A".to_string()), Some("1".to_string())],
                vec![Some("a".to_string()), Some("0".to_string())],
            ]
        );

        let resumed = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(resumed.resumed);
        assert!(resumed.complete);
        assert_eq!(resumed.rows_updated, 1);

        actor.set_mode(Mode::CreatorUp).await.unwrap();
        let completed = actor
            .query("SELECT count(*) FROM collation_keys WHERE done = 1")
            .await
            .unwrap();
        assert_eq!(completed[0][0].as_deref(), Some("2"));
    }

    #[compio::test]
    async fn exact_unique_candidate_is_accepted_but_reordered_or_nullable_tuples_are_not() {
        let (_directory, actor) = open_actor("candidate");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE candidates (\
                    id INTEGER PRIMARY KEY, tenant TEXT NOT NULL, sequence INTEGER NOT NULL, \
                    done INTEGER NOT NULL DEFAULT 0, UNIQUE (tenant, sequence)); \
                 INSERT INTO candidates (id, tenant, sequence) VALUES \
                    (1, 'a', 1), (2, 'a', 2), (3, 'b', 1); \
                 CREATE TABLE nullable_composite (\
                    id INTEGER, tenant TEXT, done INTEGER NOT NULL DEFAULT 0, \
                    PRIMARY KEY (id, tenant)); \
                 INSERT INTO nullable_composite (id, tenant) VALUES (1, 'a')",
            )
            .await
            .unwrap();
        let exact = external_spec("candidates", &["tenant", "sequence"], 2);
        let outcome = run_backfill_bounded(
            &actor,
            &exact,
            &exact.set_clause,
            exact.filter.as_deref(),
            "tester",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(outcome.complete);
        assert_eq!(outcome.rows_updated, 3);

        let reordered = external_spec("candidates", &["sequence", "tenant"], 2);
        assert!(matches!(
            run_backfill_bounded(
                &actor,
                &reordered,
                &reordered.set_clause,
                reordered.filter.as_deref(),
                "tester",
                None,
                None,
            )
            .await,
            Err(BackfillError::CursorTupleUnavailable { .. })
        ));
        let nullable = external_spec("nullable_composite", &["id", "tenant"], 2);
        assert!(matches!(
            run_backfill_bounded(
                &actor,
                &nullable,
                &nullable.set_clause,
                nullable.filter.as_deref(),
                "tester",
                None,
                None,
            )
            .await,
            Err(BackfillError::CursorTupleUnavailable { .. })
        ));
    }

    #[compio::test]
    async fn bounded_cohort_excludes_later_tuples() {
        let (_directory, actor) = open_actor("bounded");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE bounded (id INTEGER PRIMARY KEY, done INTEGER NOT NULL DEFAULT 0); \
                 INSERT INTO bounded (id) VALUES (1), (2), (3)",
            )
            .await
            .unwrap();
        let spec = external_spec("bounded", &["id"], 2);
        run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            Some(1),
            None,
        )
        .await
        .unwrap();
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec("INSERT INTO bounded (id) VALUES (4), (5)")
            .await
            .unwrap();
        let resumed = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(resumed.complete);
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        let rows = actor
            .query(
                "SELECT \
                    (SELECT count(*) FROM bounded WHERE id <= 3 AND done = 1), \
                    (SELECT count(*) FROM bounded WHERE id > 3 AND done = 0)",
            )
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("3"));
        assert_eq!(rows[0][1].as_deref(), Some("2"));
    }

    #[compio::test]
    async fn failed_checkpoint_advance_rolls_back_data_writes() {
        let (directory, actor) = open_actor("atomic");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE atomic_items (id INTEGER PRIMARY KEY, done INTEGER NOT NULL DEFAULT 0); \
                 INSERT INTO atomic_items (id) VALUES (1), (2)",
            )
            .await
            .unwrap();
        let spec = external_spec("atomic_items", &["id"], 2);
        let initialized = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            Some(0),
            None,
        )
        .await
        .unwrap();
        assert!(!initialized.complete);

        let journal = rusqlite::Connection::open(directory.path().join("atomic.journal.sqlite"))
            .expect("open journal directly");
        journal
            .execute_batch(
                "CREATE TRIGGER abort_checkpoint BEFORE UPDATE OF last_cursor ON schema_backfills \
                 BEGIN SELECT RAISE(ABORT, 'checkpoint blocked'); END",
            )
            .unwrap();
        drop(journal);

        let error = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
            None,
        )
        .await
        .expect_err("checkpoint failure must abort the data transaction");
        assert!(error.to_string().contains("checkpoint blocked"), "{error}");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        let rows = actor
            .query("SELECT count(*) FROM atomic_items WHERE done <> 0")
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("0"));
        actor.set_mode(Mode::EngineJournal).await.unwrap();
        let progress = actor
            .query("SELECT last_cursor, rows_done, batches_done FROM \"_mig\".schema_backfills")
            .await
            .unwrap();
        assert_eq!(progress[0], vec![None, Some("0".into()), Some("0".into())]);
    }

    #[compio::test]
    async fn resume_refuses_cursor_columns_and_cohort_bound_drift() {
        let (directory, actor) = open_actor("drift");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE drift_items (id INTEGER PRIMARY KEY, done INTEGER NOT NULL DEFAULT 0); \
                 INSERT INTO drift_items (id) VALUES (1), (2)",
            )
            .await
            .unwrap();
        let spec = external_spec("drift_items", &["id"], 1);
        run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            Some(0),
            None,
        )
        .await
        .unwrap();
        let journal = rusqlite::Connection::open(directory.path().join("drift.journal.sqlite"))
            .expect("open journal directly");
        journal
            .execute(
                "UPDATE schema_backfills SET cursor_columns = '[\"other\"]'",
                [],
            )
            .unwrap();
        drop(journal);
        let error = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
            None,
        )
        .await
        .expect_err("stored tuple drift must refuse resume");
        assert!(error.to_string().contains("progress drift"), "{error}");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        let rows = actor
            .query("SELECT count(*) FROM drift_items WHERE done <> 0")
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("0"));
    }

    #[compio::test]
    async fn guard_is_durable_rejects_each_component_and_is_cleaned_at_completion() {
        let (_directory, actor) = open_actor("guard");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE guarded (\
                    tenant TEXT COLLATE NOCASE NOT NULL, sequence INTEGER NOT NULL, \
                    done INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (tenant, sequence)); \
                 INSERT INTO guarded (tenant, sequence) VALUES ('a', 1), ('a', 2), ('b', 1)",
            )
            .await
            .unwrap();
        let mut spec = external_spec("guarded", &["tenant", "sequence"], 1);
        spec.cursor_stability = CursorStability::GuardUpdates;
        let interrupted = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            Some(1),
            None,
        )
        .await
        .unwrap();
        assert!(!interrupted.complete);

        actor.set_mode(Mode::EngineJournal).await.unwrap();
        let progress = actor
            .query("SELECT guard_installed, guard_cleaned, complete FROM \"_mig\".schema_backfills")
            .await
            .unwrap();
        assert_eq!(
            progress[0],
            vec![Some("1".into()), Some("0".into()), Some("0".into())]
        );
        let triggers = actor
            .query("SELECT count(*) FROM main.sqlite_schema WHERE type = 'trigger'")
            .await
            .unwrap();
        assert_eq!(triggers[0][0].as_deref(), Some("1"));

        actor.set_mode(Mode::CreatorUp).await.unwrap();
        for sql in [
            "UPDATE guarded SET tenant = 'z' WHERE tenant = 'a' AND sequence = 2",
            "UPDATE guarded SET tenant = 'A' WHERE tenant = 'a' AND sequence = 2",
            "UPDATE guarded SET sequence = 9 WHERE tenant = 'a' AND sequence = 2",
        ] {
            let error = actor
                .exec(sql)
                .await
                .expect_err("guard must reject mutation");
            assert!(
                error.to_string().contains("cursor stability guard"),
                "{error}"
            );
        }

        let resumed = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(resumed.complete);
        actor.set_mode(Mode::EngineJournal).await.unwrap();
        let progress = actor
            .query("SELECT guard_installed, guard_cleaned, complete FROM \"_mig\".schema_backfills")
            .await
            .unwrap();
        assert_eq!(
            progress[0],
            vec![Some("1".into()), Some("1".into()), Some("1".into())]
        );
        let triggers = actor
            .query("SELECT count(*) FROM main.sqlite_schema WHERE type = 'trigger'")
            .await
            .unwrap();
        assert_eq!(triggers[0][0].as_deref(), Some("0"));
    }

    #[compio::test]
    async fn existing_target_trigger_is_rejected_before_guard_or_cohort_capture() {
        let (_directory, actor) = open_actor("trigger_interaction");
        actor.set_mode(Mode::CreatorUp).await.unwrap();
        actor
            .exec(
                "CREATE TABLE trigger_items (id INTEGER PRIMARY KEY, done INTEGER NOT NULL DEFAULT 0); \
                 INSERT INTO trigger_items (id) VALUES (1); \
                 CREATE TRIGGER application_trigger AFTER UPDATE ON trigger_items \
                 BEGIN SELECT 1; END",
            )
            .await
            .unwrap();
        let mut spec = external_spec("trigger_items", &["id"], 1);
        spec.cursor_stability = CursorStability::GuardUpdates;
        let error = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
            None,
        )
        .await
        .expect_err("an unproven trigger interaction must fail closed");
        assert!(error.to_string().contains("trigger"), "{error}");
        actor.set_mode(Mode::EngineJournal).await.unwrap();
        let rows = actor
            .query("SELECT count(*) FROM \"_mig\".schema_backfills")
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("0"));
    }
}
