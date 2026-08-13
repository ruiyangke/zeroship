//! PostgreSQL cursor-paged, crash-safe backfills over [`SqlSession`].
//!
//! Each batch updates a bounded key window and advances its progress cursor in
//! the same transaction. The outer plan orchestrator holds the project lock for
//! the whole ordered plan; batch transactions bound row locks and WAL growth.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::apply::backend::{BackfillError, BackfillOutcome, BackfillProgressEntry, BackfillSpec};
use crate::apply::executor::ApplyError;
use crate::apply::journal::{self, JournalError};
use crate::apply::timeout::resolve_timeout_ms;
use crate::approval::Approval;
use crate::conn::ExecutorConfig;
use crate::driver::{Bind, Row, SqlSession};
use crate::guard::SqlGuard;
use crate::model::backfill::{
    generate_per_row_value, CursorColumnContract, CursorComparison, CursorContract,
    CursorScalarType, CursorTuple,
};
use crate::model::ir::{CursorStability, IrScalar, PerRowGenerator};
use crate::model::migration::{Checksum, MigrationId};

use super::session::AUTHOR_SQL_LITERAL_MODE;

const CURSOR_SESSION_SETTINGS: &str =
    "SET LOCAL DateStyle TO 'ISO, YMD'; SET LOCAL TimeZone TO 'UTC';";

fn backend_error(message: impl Into<String>) -> ApplyError {
    ApplyError::Backend(format!("postgres backfill: {}", message.into()))
}

fn validate_ident(what: &str, value: &str) -> Result<(), ApplyError> {
    let mut chars = value.chars();
    let valid_first = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if value.is_empty() || !valid_first || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(backend_error(format!(
            "invalid {what} identifier {value:?}; expected [A-Za-z_][A-Za-z0-9_]*"
        )));
    }
    Ok(())
}

fn quote_ident(value: &str) -> Result<String, ApplyError> {
    Ok(crate::render::dml::quote_ident_checked(value)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PgCursorColumn {
    contract: CursorColumnContract,
    quoted_name: String,
    cast_type: String,
    collation_sql: Option<String>,
}

impl PgCursorColumn {
    fn source_expr(&self, alias: &str) -> String {
        self.with_collation(format!("{alias}.{}", self.quoted_name))
    }

    fn unqualified_expr(&self) -> String {
        self.with_collation(self.quoted_name.clone())
    }

    fn bind_expr(&self, parameter: usize) -> String {
        self.with_collation(format!("(${parameter}::text)::{}", self.cast_type))
    }

    fn with_collation(&self, expression: String) -> String {
        self.collation_sql.as_ref().map_or_else(
            || expression.clone(),
            |collation| format!("{expression} {collation}"),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PgCursor {
    contract: CursorContract,
    columns: Vec<PgCursorColumn>,
}

impl PgCursor {
    #[cfg(test)]
    fn from_contract(contract: CursorContract) -> Result<Self, ApplyError> {
        let columns = contract
            .columns
            .iter()
            .cloned()
            .map(|column| {
                let quoted_name = quote_ident(&column.name)?;
                let cast_type = pg_cursor_cast_type(&column, None, None)?;
                let collation_sql = pg_collation_sql(&column)?;
                Ok(PgCursorColumn {
                    contract: column,
                    quoted_name,
                    cast_type,
                    collation_sql,
                })
            })
            .collect::<Result<Vec<_>, ApplyError>>()?;
        Ok(Self { contract, columns })
    }

    fn arity(&self) -> usize {
        self.columns.len()
    }
}

fn cursor_tuple_unavailable(spec: &BackfillSpec, reason: impl Into<String>) -> ApplyError {
    backend_error(
        BackfillError::CursorTupleUnavailable {
            table: format!("{}.{}", spec.schema, spec.table),
            cursor_columns: spec.cursor_columns.clone(),
            reason: reason.into(),
        }
        .to_string(),
    )
}

fn cursor_component_mutated(component: &str) -> ApplyError {
    backend_error(
        BackfillError::CursorComponentMutated {
            cursor_component: component.to_string(),
        }
        .to_string(),
    )
}

fn pg_collation_sql(column: &CursorColumnContract) -> Result<Option<String>, ApplyError> {
    match &column.comparison {
        CursorComparison::Default | CursorComparison::CaseInsensitive => Ok(None),
        CursorComparison::NamedCollation { schema, name } => {
            let schema = schema.as_deref().ok_or_else(|| {
                backend_error(format!(
                    "PostgreSQL cursor component {:?} has an unqualified named collation",
                    column.name
                ))
            })?;
            Ok(Some(format!(
                "COLLATE {}.{}",
                quote_ident(schema)?,
                quote_ident(name)?
            )))
        }
        CursorComparison::MysqlText { .. } => Err(backend_error(format!(
            "PostgreSQL cursor component {:?} carries MySQL comparison semantics",
            column.name
        ))),
    }
}

fn pg_cursor_cast_type(
    column: &CursorColumnContract,
    type_schema: Option<&str>,
    type_name: Option<&str>,
) -> Result<String, ApplyError> {
    let ty = column.database_type.trim().to_ascii_lowercase();
    if ty == "citext" {
        let schema = type_schema.ok_or_else(|| {
            backend_error(format!(
                "cursor component {:?} is citext but its type schema is unavailable",
                column.name
            ))
        })?;
        let name = type_name
            .filter(|name| name.eq_ignore_ascii_case("citext"))
            .ok_or_else(|| {
                backend_error(format!(
                    "cursor component {:?} no longer resolves to citext",
                    column.name
                ))
            })?;
        return Ok(format!("{}.{}", quote_ident(schema)?, quote_ident(name)?));
    }

    let base = ty
        .split_once('(')
        .map_or(ty.as_str(), |(base, _)| base.trim());
    let supported = matches!(
        base,
        "smallint"
            | "integer"
            | "bigint"
            | "int2"
            | "int4"
            | "int8"
            | "numeric"
            | "decimal"
            | "text"
            | "character"
            | "character varying"
            | "char"
            | "varchar"
            | "uuid"
            | "date"
            | "time"
            | "time without time zone"
            | "time with time zone"
            | "timestamp"
            | "timestamp without time zone"
            | "timestamp with time zone"
            | "timestamptz"
    );
    let modifiers_safe = ty.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, ' ' | '(' | ')' | ',')
    });
    if !supported || !modifiers_safe {
        return Err(backend_error(format!(
            "cursor component {:?} has unsupported PostgreSQL paging type {:?}",
            column.name, column.database_type
        )));
    }
    Ok(ty)
}

fn scalar_from_pg_text(
    value: String,
    scalar_type: CursorScalarType,
) -> Result<IrScalar, ApplyError> {
    match scalar_type {
        CursorScalarType::Int64 => value.parse::<i64>().map(IrScalar::Int64).map_err(|error| {
            backend_error(format!("invalid int64 cursor value {value:?}: {error}"))
        }),
        CursorScalarType::Decimal => Ok(IrScalar::Decimal(value)),
        CursorScalarType::String => Ok(IrScalar::Str(value)),
    }
}

fn tuple_from_row(row: &Row, prefix: &str, cursor: &PgCursor) -> Result<CursorTuple, ApplyError> {
    let values = cursor
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let field = format!("{prefix}{index}");
            let value: Option<String> = row.try_get(field.as_str())?;
            let value = value.ok_or_else(|| {
                backend_error(format!(
                    "cursor component {:?} unexpectedly decoded as NULL",
                    column.contract.name
                ))
            })?;
            scalar_from_pg_text(value, column.contract.scalar_type)
        })
        .collect::<Result<Vec<_>, ApplyError>>()?;
    CursorTuple::new(values, &cursor.contract)
        .map_err(|error| backend_error(format!("invalid typed cursor tuple: {error}")))
}

fn tuple_binds(tuple: &CursorTuple) -> Vec<Bind> {
    tuple
        .values()
        .iter()
        .map(|value| match value {
            IrScalar::Int(value) | IrScalar::Int64(value) => Bind::Text(value.to_string()),
            IrScalar::Decimal(value) | IrScalar::Str(value) => Bind::Text(value.clone()),
            IrScalar::Null | IrScalar::Bool(_) | IrScalar::Bytes(_) => {
                unreachable!("CursorTuple rejects unsupported cursor scalar families")
            }
        })
        .collect()
}

fn lexicographic_gt(cursor: &PgCursor, alias: &str, first_parameter: usize) -> String {
    cursor
        .columns
        .iter()
        .enumerate()
        .map(|(position, column)| {
            let mut terms = cursor.columns[..position]
                .iter()
                .enumerate()
                .map(|(prefix, prefix_column)| {
                    format!(
                        "{} = {}",
                        prefix_column.source_expr(alias),
                        prefix_column.bind_expr(first_parameter + prefix)
                    )
                })
                .collect::<Vec<_>>();
            terms.push(format!(
                "{} > {}",
                column.source_expr(alias),
                column.bind_expr(first_parameter + position)
            ));
            format!("({})", terms.join(" AND "))
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn batch_binds(last_cursor: Option<&CursorTuple>, end_cursor: &CursorTuple) -> Vec<Bind> {
    let mut binds = Vec::new();
    if let Some(last_cursor) = last_cursor {
        binds.extend(tuple_binds(last_cursor));
    }
    binds.extend(tuple_binds(end_cursor));
    binds
}

fn authored_filter(spec: &BackfillSpec) -> String {
    spec.filter
        .as_deref()
        .map(|value| format!(" AND ({value})"))
        .unwrap_or_default()
}

fn build_end_cursor_sql(spec: &BackfillSpec, cursor: &PgCursor) -> Result<String, ApplyError> {
    let schema = quote_ident(&spec.schema)?;
    let table = quote_ident(&spec.table)?;
    let projections = cursor
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| format!("{}::text AS _bf_end_{index}", column.unqualified_expr()))
        .collect::<Vec<_>>()
        .join(", ");
    let order = cursor
        .columns
        .iter()
        .map(PgCursorColumn::unqualified_expr)
        .map(|expression| format!("{expression} DESC"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "SELECT {projections} FROM {schema}.{table} \
         WHERE TRUE{} ORDER BY {order} LIMIT 1",
        authored_filter(spec)
    ))
}

fn build_batch_sql(
    spec: &BackfillSpec,
    cursor: &PgCursor,
    have_cursor: bool,
) -> Result<String, ApplyError> {
    let schema = quote_ident(&spec.schema)?;
    let table = quote_ident(&spec.table)?;
    let last_start = 1;
    let end_start = if have_cursor { cursor.arity() + 1 } else { 1 };
    let after = if have_cursor {
        format!("({})", lexicographic_gt(cursor, "_bf_source", last_start))
    } else {
        "TRUE".to_string()
    };
    let bounded = format!(
        "NOT ({})",
        lexicographic_gt(cursor, "_bf_source", end_start)
    );
    let window_projection = cursor
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| format!("{} AS _bf_key_{index}", column.source_expr("_bf_source")))
        .collect::<Vec<_>>()
        .join(", ");
    let window_order = cursor
        .columns
        .iter()
        .map(|column| format!("{} ASC", column.source_expr("_bf_source")))
        .collect::<Vec<_>>()
        .join(", ");
    let update_match = cursor
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| format!("_bf.{} = _bf_window._bf_key_{index}", column.quoted_name))
        .collect::<Vec<_>>()
        .join(" AND ");
    let reverse_window_order = (0..cursor.arity())
        .map(|index| format!("_bf_key_{index} DESC"))
        .collect::<Vec<_>>()
        .join(", ");
    // The inner `::text` MUST carry an alias that cannot collide with the key it
    // is cast from. PostgreSQL names a bare cast expression after its underlying
    // column, so `SELECT _bf_key_0::text ... ORDER BY _bf_key_0 DESC` produces an
    // OUTPUT column also called `_bf_key_0` - and `ORDER BY` resolves an output
    // name in preference to an input one, so it sorted by TEXT.
    //
    // That made each batch checkpoint at the TEXT-maximum of its window. A window
    // holding 9 and 10 checkpointed at '9', so the saved cursor moved BACKWARDS,
    // the next batch re-selected row 10, and a non-idempotent transform ran on it
    // twice - silently, with the migration reporting success. The same disagreement
    // occurs at every decade boundary (99/100, 999/1000) and for any pair like
    // 2/10.
    //
    // `build_end_cursor_sql` above already aliases its cast (`AS _bf_end_{index}`)
    // and was never affected; this is that same convention, and the reason to keep
    // it is that the bug is invisible without it.
    let returned_cursor = (0..cursor.arity())
        .map(|index| {
            format!(
                "(SELECT _bf_key_{index}::text AS _bf_cursor_text_{index} FROM _bf_window \
                  ORDER BY {reverse_window_order} LIMIT 1) AS _bf_cursor_{index}"
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    Ok(format!(
        "WITH _bf_window AS ( \
             SELECT {window_projection} \
               FROM {schema}.{table} AS _bf_source \
              WHERE {after} AND {bounded}{} \
              ORDER BY {window_order} LIMIT {batch_size} \
         ), _bf_updated AS ( \
             UPDATE {schema}.{table} AS _bf SET {set_clause} \
               FROM _bf_window WHERE {update_match} RETURNING 1 \
         ) \
         SELECT (SELECT count(*) FROM _bf_window) AS _bf_selected, \
                (SELECT count(*) FROM _bf_updated) AS _bf_rows, \
                {returned_cursor}",
        authored_filter(spec),
        batch_size = spec.batch_size,
        set_clause = spec.set_clause,
    ))
}

fn build_per_row_window_sql(
    spec: &BackfillSpec,
    cursor: &PgCursor,
    have_cursor: bool,
) -> Result<String, ApplyError> {
    let schema = quote_ident(&spec.schema)?;
    let table = quote_ident(&spec.table)?;
    let end_start = if have_cursor { cursor.arity() + 1 } else { 1 };
    let after = if have_cursor {
        format!("({})", lexicographic_gt(cursor, "_bf_source", 1))
    } else {
        "TRUE".to_string()
    };
    let bounded = format!(
        "NOT ({})",
        lexicographic_gt(cursor, "_bf_source", end_start)
    );
    let projection = cursor
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            format!(
                "{}::text AS _bf_cursor_{index}",
                column.source_expr("_bf_source")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let order = cursor
        .columns
        .iter()
        .map(|column| format!("{} ASC", column.source_expr("_bf_source")))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "SELECT {projection} FROM {schema}.{table} AS _bf_source \
          WHERE {after} AND {bounded}{} \
          ORDER BY {order} LIMIT {batch_size} FOR UPDATE",
        authored_filter(spec),
        batch_size = spec.batch_size,
    ))
}

fn build_per_row_update_sql(spec: &BackfillSpec, cursor: &PgCursor) -> Result<String, ApplyError> {
    let schema = quote_ident(&spec.schema)?;
    let table = quote_ident(&spec.table)?;
    let mut assignments = Vec::with_capacity(spec.per_row.len() + 1);
    if !spec.set_clause.trim().is_empty() {
        assignments.push(spec.set_clause.clone());
    }
    for (index, column) in spec.per_row.keys().enumerate() {
        assignments.push(format!("{} = ${}", quote_ident(column)?, index + 1));
    }
    let first_cursor_parameter = spec.per_row.len() + 1;
    let predicate = cursor
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            format!(
                "{} = {}",
                column.source_expr("_bf"),
                column.bind_expr(first_cursor_parameter + index)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    Ok(format!(
        "UPDATE {schema}.{table} AS _bf SET {} WHERE {predicate}",
        assignments.join(", ")
    ))
}

fn walk_update_targets(value: &Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match value {
        Value::Object(map) => {
            if let Some(update) = map.get("UpdateStmt") {
                if let Some(Value::Array(targets)) = update.get("target_list") {
                    for target in targets {
                        let result = target
                            .get("ResTarget")
                            .or_else(|| target.get("node").and_then(|node| node.get("ResTarget")));
                        if let Some(name) = result
                            .and_then(|node| node.get("name"))
                            .and_then(Value::as_str)
                        {
                            if !name.is_empty() && visit(name) {
                                return true;
                            }
                        }
                    }
                }
            }
            map.values().any(|child| walk_update_targets(child, visit))
        }
        Value::Array(items) => items.iter().any(|item| walk_update_targets(item, visit)),
        _ => false,
    }
}

fn assert_cursor_not_mutated(sql: &str, cursor_columns: &[String]) -> Result<(), ApplyError> {
    let parsed = pg_query::parse(sql)
        .map_err(|error| backend_error(format!("could not parse assembled SQL: {error}")))?;
    let tree = serde_json::to_value(&parsed.protobuf)
        .map_err(|error| backend_error(format!("could not inspect assembled SQL: {error}")))?;
    let mut mutated = None;
    walk_update_targets(&tree, &mut |name| {
        if let Some(component) = cursor_columns
            .iter()
            .find(|component| component.as_str() == name)
        {
            mutated = Some(component.clone());
            true
        } else {
            false
        }
    });
    match mutated {
        Some(component) => Err(cursor_component_mutated(&component)),
        None => Ok(()),
    }
}

fn validate_spec(spec: &BackfillSpec) -> Result<Option<&CursorContract>, ApplyError> {
    validate_ident("schema", &spec.schema)?;
    validate_ident("table", &spec.table)?;
    if spec.cursor_columns.is_empty() {
        return Err(cursor_tuple_unavailable(spec, "cursorColumns is empty"));
    }
    for column in &spec.cursor_columns {
        validate_ident("cursor component", column)?;
    }
    if spec.batch_size == 0 {
        return Err(backend_error(BackfillError::InvalidBatchSize.to_string()));
    }
    if spec.set_clause.trim().is_empty() && spec.per_row.is_empty() {
        return Err(backend_error("backfill set must not be empty"));
    }
    if let Some(contract) = &spec.cursor_contract {
        contract
            .validate_columns(&spec.cursor_columns)
            .map_err(|error| cursor_tuple_unavailable(spec, error.to_string()))?;
        if contract.columns.len() != spec.cursor_columns.len() {
            return Err(cursor_tuple_unavailable(
                spec,
                "cursor contract arity does not match cursorColumns",
            ));
        }
    }
    for (column, assignment) in &spec.per_row {
        validate_ident("per-row destination column", column)?;
        if let Some(component) = spec
            .cursor_columns
            .iter()
            .find(|component| component.as_str() == column)
        {
            return Err(cursor_component_mutated(component));
        }
        if !assignment.matches_target(&spec.schema, &spec.table, column) {
            return Err(backend_error(format!(
                "per-row assignment for destination {column:?} was validated for a different target"
            )));
        }
        if let PerRowGenerator::TypeId { prefix } = assignment.generator() {
            crate::model::ir::validate_type_id_prefix(prefix).map_err(|error| {
                backend_error(format!(
                    "invalid TypeID prefix for per-row destination {column:?}: {error}"
                ))
            })?;
        }
    }
    Ok(spec.cursor_contract.as_ref())
}

const CURSOR_COLUMN_SQL: &str = "SELECT c.data_type, c.udt_schema, c.udt_name, \
            column_type.typtype::text AS type_kind, \
            c.character_maximum_length::bigint AS character_maximum_length, \
            c.collation_schema, c.collation_name, a.attnotnull, \
            a.attgenerated::text AS generated_kind \
       FROM information_schema.columns c \
       JOIN pg_catalog.pg_namespace n ON n.nspname = c.table_schema \
       JOIN pg_catalog.pg_class rel \
         ON rel.relnamespace = n.oid AND rel.relname = c.table_name \
       JOIN pg_catalog.pg_attribute a \
         ON a.attrelid = rel.oid AND a.attname = c.column_name \
       JOIN pg_catalog.pg_type column_type ON column_type.oid = a.atttypid \
      WHERE c.table_schema = $1 AND c.table_name = $2 \
        AND c.column_name = $3 \
        AND a.attnum > 0 AND NOT a.attisdropped";

const CANDIDATE_KEY_SQL: &str = "SELECT idx.relname AS index_name, \
            array_agg(a.attname::text ORDER BY key.ordinality) AS key_columns, \
            bool_and(opc.opcdefault \
                AND i.indcollation[(key.ordinality - 1)::integer] = a.attcollation) \
                AS default_semantics \
       FROM pg_catalog.pg_class rel \
       JOIN pg_catalog.pg_namespace n ON n.oid = rel.relnamespace \
       JOIN pg_catalog.pg_index i ON i.indrelid = rel.oid \
       JOIN pg_catalog.pg_class idx ON idx.oid = i.indexrelid \
       JOIN pg_catalog.pg_am am ON am.oid = idx.relam \
       CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS key(attnum, ordinality) \
       JOIN pg_catalog.pg_attribute a \
         ON a.attrelid = rel.oid AND a.attnum = key.attnum \
       JOIN pg_catalog.pg_opclass opc \
         ON opc.oid = i.indclass[(key.ordinality - 1)::integer] \
      WHERE n.nspname = $1 AND rel.relname = $2 \
        AND rel.relkind IN ('r', 'p') \
        AND (i.indisprimary OR i.indisunique) \
        AND i.indisvalid AND i.indisready \
        AND i.indpred IS NULL AND i.indexprs IS NULL \
        AND am.amname = 'btree' \
        AND i.indnkeyatts::bigint = $3::bigint \
        AND key.ordinality <= i.indnkeyatts \
      GROUP BY i.indexrelid, idx.relname";

const TARGET_KIND_SQL: &str = "SELECT c.relkind::text AS relkind, c.relhassubclass \
       FROM pg_catalog.pg_class c \
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
      WHERE n.nspname = $1 AND c.relname = $2 \
        AND c.relkind IN ('r', 'p')";

const ENABLED_USER_TRIGGER_SQL: &str = "WITH RECURSIVE target(relid) AS ( \
            SELECT c.oid \
              FROM pg_catalog.pg_class c \
              JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 \
               AND c.relkind IN ('r', 'p') \
            UNION ALL \
            SELECT i.inhrelid FROM pg_catalog.pg_inherits i \
              JOIN target parent ON parent.relid = i.inhparent \
        ) \
        SELECT ns.nspname AS trigger_schema, rel.relname AS trigger_table, \
               t.tgname AS trigger_name, t.tgtype::bigint AS trigger_type, \
               t.tgenabled::text AS trigger_enabled, \
               (t.tgqual IS NULL) AS no_when_clause, \
               t.tgnargs::bigint AS trigger_arg_count, \
               pn.nspname AS function_schema, p.proname AS function_name, \
               p.pronargs::bigint AS function_arg_count, \
               l.lanname AS function_language, p.prosecdef AS security_definer, \
               COALESCE(p.proconfig, ARRAY[]::text[]) AS function_config, \
               p.prosrc AS function_body \
          FROM target \
          JOIN pg_catalog.pg_class rel ON rel.oid = target.relid \
          JOIN pg_catalog.pg_namespace ns ON ns.oid = rel.relnamespace \
          JOIN pg_catalog.pg_trigger t ON t.tgrelid = target.relid \
          JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
          JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace \
          JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
         WHERE NOT t.tgisinternal AND t.tgenabled <> 'D'";

fn pg_cursor_scalar_type(database_type: &str) -> Option<CursorScalarType> {
    let ty = database_type.to_ascii_lowercase();
    let base = ty
        .split_once('(')
        .map_or(ty.as_str(), |(base, _)| base.trim());
    if matches!(
        base,
        "smallint" | "integer" | "bigint" | "int2" | "int4" | "int8"
    ) {
        Some(CursorScalarType::Int64)
    } else if matches!(base, "numeric" | "decimal") {
        Some(CursorScalarType::Decimal)
    } else if matches!(
        base,
        "text"
            | "citext"
            | "character"
            | "character varying"
            | "char"
            | "varchar"
            | "uuid"
            | "date"
            | "time"
            | "time without time zone"
            | "time with time zone"
            | "timestamp"
            | "timestamp without time zone"
            | "timestamp with time zone"
            | "timestamptz"
    ) {
        Some(CursorScalarType::String)
    } else {
        None
    }
}

fn normalize_catalog_type(
    data_type: &str,
    udt_name: &str,
    type_kind: &str,
    character_maximum_length: Option<i64>,
) -> Result<String, String> {
    if type_kind == "d" {
        return Err("domain cursor types are not supported".to_string());
    }
    if data_type.eq_ignore_ascii_case("USER-DEFINED") {
        if udt_name.eq_ignore_ascii_case("citext") {
            return Ok("citext".to_string());
        }
        return Err(format!(
            "user-defined cursor type {udt_name:?} is unsupported"
        ));
    }
    let normalized = data_type
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if normalized == "character" {
        if let Some(length) = character_maximum_length.filter(|length| *length > 0) {
            return Ok(format!("character({length})"));
        }
    }
    Ok(normalized)
}

fn reject_generated_cursor_column(
    row: &Row,
    spec: &BackfillSpec,
    name: &str,
) -> Result<(), ApplyError> {
    let generated_kind: String = row.try_get("generated_kind")?;
    if generated_kind.is_empty() {
        Ok(())
    } else {
        Err(cursor_tuple_unavailable(
            spec,
            format!(
                "cursor component {name:?} is generated; dependency-driven value changes cannot be guarded by an UPDATE OF cursor trigger"
            ),
        ))
    }
}

async fn inspect_cursor<D: SqlSession>(
    conn: &D,
    spec: &BackfillSpec,
) -> Result<PgCursor, ApplyError> {
    let expected = validate_spec(spec)?.cloned();
    let mut actual_columns = Vec::with_capacity(spec.cursor_columns.len());
    let mut type_identities = Vec::with_capacity(spec.cursor_columns.len());

    for name in &spec.cursor_columns {
        let rows = conn
            .query(
                CURSOR_COLUMN_SQL,
                &[
                    Bind::Text(spec.schema.clone()),
                    Bind::Text(spec.table.clone()),
                    Bind::Text(name.clone()),
                ],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(backend_error(
                BackfillError::TargetNotFound(format!(
                    "{}.{} cursor component {name:?}",
                    spec.schema, spec.table
                ))
                .to_string(),
            ));
        };
        reject_generated_cursor_column(row, spec, name)?;
        let not_null: bool = row.try_get("attnotnull")?;
        if !not_null {
            return Err(cursor_tuple_unavailable(
                spec,
                format!("cursor component {name:?} is nullable"),
            ));
        }
        let data_type: String = row.try_get("data_type")?;
        let udt_schema: String = row.try_get("udt_schema")?;
        let udt_name: String = row.try_get("udt_name")?;
        let type_kind: String = row.try_get("type_kind")?;
        let length: Option<i64> = row.try_get("character_maximum_length")?;
        let database_type = normalize_catalog_type(&data_type, &udt_name, &type_kind, length)
            .map_err(|reason| cursor_tuple_unavailable(spec, reason))?;
        let scalar_type = pg_cursor_scalar_type(&database_type).ok_or_else(|| {
            cursor_tuple_unavailable(
                spec,
                format!("cursor component {name:?} has unsupported ordered type {database_type:?}"),
            )
        })?;
        let collation_name: Option<String> = row.try_get("collation_name")?;
        let comparison = if let Some(collation_name) = collation_name {
            CursorComparison::NamedCollation {
                schema: row.try_get("collation_schema")?,
                name: collation_name,
            }
        } else if database_type == "citext" {
            CursorComparison::CaseInsensitive
        } else {
            CursorComparison::Default
        };
        actual_columns.push(CursorColumnContract {
            name: name.clone(),
            scalar_type,
            database_type,
            comparison,
        });
        type_identities.push((udt_schema, udt_name));
    }

    let actual = CursorContract {
        columns: actual_columns,
    };
    if expected
        .as_ref()
        .is_some_and(|expected| actual != *expected)
    {
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "live cursor type/collation contract drifted: planned {expected:?}, live {actual:?}"
            ),
        ));
    }

    let arity = i64::try_from(spec.cursor_columns.len())
        .map_err(|_| cursor_tuple_unavailable(spec, "cursor tuple arity exceeds i64"))?;
    let candidates = conn
        .query(
            CANDIDATE_KEY_SQL,
            &[
                Bind::Text(spec.schema.clone()),
                Bind::Text(spec.table.clone()),
                Bind::Int(arity),
            ],
        )
        .await?;
    let candidate = candidates.iter().find(|row| {
        let columns = row.try_get::<_, Vec<String>>("key_columns");
        let semantics = row.try_get::<_, bool>("default_semantics");
        matches!((columns, semantics), (Ok(columns), Ok(true)) if columns == spec.cursor_columns)
    });
    if candidate.is_none() {
        return Err(cursor_tuple_unavailable(
            spec,
            "the exact ordered tuple is not a valid/ready PRIMARY or UNIQUE default-B-tree key with compatible opclasses and collations",
        ));
    }

    let columns = actual
        .columns
        .iter()
        .cloned()
        .zip(type_identities)
        .map(|(column, (type_schema, type_name))| {
            Ok(PgCursorColumn {
                quoted_name: quote_ident(&column.name)?,
                cast_type: pg_cursor_cast_type(
                    &column,
                    Some(type_schema.as_str()),
                    Some(type_name.as_str()),
                )?,
                collation_sql: pg_collation_sql(&column)?,
                contract: column,
            })
        })
        .collect::<Result<Vec<_>, ApplyError>>()?;
    Ok(PgCursor {
        contract: actual,
        columns,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GuardIdentity {
    trigger_name: String,
    function_name: String,
    marker: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AllowedOnlineRenameTrigger {
    trigger_name: String,
    function_name: String,
    from_column: String,
    to_column: String,
}

impl AllowedOnlineRenameTrigger {
    pub(super) fn new(
        trigger_name: String,
        function_name: String,
        from_column: String,
        to_column: String,
    ) -> Self {
        Self {
            trigger_name,
            function_name,
            from_column,
            to_column,
        }
    }
}

fn guard_identity(
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
) -> GuardIdentity {
    let mut hash = Sha256::new();
    hash.update(b"zero-migrate/postgres/cursor-guard/v1");
    hash.update(version.as_str().as_bytes());
    hash.update(checksum.as_str().as_bytes());
    for column in &spec.cursor_columns {
        hash.update((column.len() as u64).to_be_bytes());
        hash.update(column.as_bytes());
    }
    let digest = hex::encode(&hash.finalize()[..12]);
    GuardIdentity {
        trigger_name: format!("zm_cursor_guard_trg_{digest}"),
        function_name: format!("zm_cursor_guard_fn_{digest}"),
        marker: format!("zero-migrate:cursor-guard:v1:{digest}"),
    }
}

fn guard_function_body(spec: &BackfillSpec) -> Result<String, ApplyError> {
    let changed = spec
        .cursor_columns
        .iter()
        .map(|column| {
            let column = quote_ident(column)?;
            Ok(format!(
                "OLD.{column}::text COLLATE \"pg_catalog\".\"C\" IS DISTINCT FROM NEW.{column}::text COLLATE \"pg_catalog\".\"C\""
            ))
        })
        .collect::<Result<Vec<_>, ApplyError>>()?
        .join(" OR ");
    Ok(format!(
        "BEGIN\n  IF {changed} THEN\n    RAISE EXCEPTION 'zero-migrate cursor components are immutable during resumable backfill' USING ERRCODE = '55000';\n  END IF;\n  RETURN NEW;\nEND"
    ))
}

async fn ensure_guard_target_supported<D: SqlSession>(
    conn: &D,
    spec: &BackfillSpec,
) -> Result<(), ApplyError> {
    let rows = conn
        .query(
            TARGET_KIND_SQL,
            &[
                Bind::Text(spec.schema.clone()),
                Bind::Text(spec.table.clone()),
            ],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(backend_error(
            BackfillError::TargetNotFound(format!("{}.{}", spec.schema, spec.table)).to_string(),
        ));
    };
    let relkind: String = row.try_get("relkind")?;
    let has_children: bool = row.try_get("relhassubclass")?;
    if relkind != "r" || has_children {
        return Err(cursor_tuple_unavailable(
            spec,
            "guardUpdates currently requires one ordinary, non-inherited PostgreSQL table so every write path is provably guarded",
        ));
    }
    Ok(())
}

fn prove_allowed_engine_trigger(
    row: &Row,
    spec: &BackfillSpec,
    expected: &AllowedOnlineRenameTrigger,
) -> Result<(), ApplyError> {
    let trigger_type: i64 = row.try_get("trigger_type")?;
    let enabled: String = row.try_get("trigger_enabled")?;
    let no_when_clause: bool = row.try_get("no_when_clause")?;
    let trigger_arg_count: i64 = row.try_get("trigger_arg_count")?;
    let function_schema: String = row.try_get("function_schema")?;
    let function_name: String = row.try_get("function_name")?;
    let function_arg_count: i64 = row.try_get("function_arg_count")?;
    let language: String = row.try_get("function_language")?;
    let security_definer: bool = row.try_get("security_definer")?;
    let function_config: Vec<String> = row.try_get("function_config")?;
    let function_body: String = row.try_get("function_body")?;
    let from = quote_ident(&expected.from_column)?;
    let to = quote_ident(&expected.to_column)?;
    let expected_body = crate::render::expand_contract::dual_write_function_body(&from, &to);
    let touches_cursor = spec
        .cursor_columns
        .iter()
        .any(|column| column == &expected.from_column || column == &expected.to_column);
    if trigger_type != 23
        || !matches!(enabled.as_str(), "O" | "A")
        || !no_when_clause
        || trigger_arg_count != 0
        || function_schema != spec.schema
        || function_name != expected.function_name
        || function_arg_count != 0
        || language != "plpgsql"
        || security_definer
        || !function_config.is_empty()
        || function_body != expected_body
        || touches_cursor
    {
        return Err(backend_error(
            "the allowed online-rename trigger no longer has the proven zero-migrate dual-write shape or touches a cursor component",
        ));
    }
    Ok(())
}

async fn reject_trigger_interactions<D: SqlSession>(
    conn: &D,
    spec: &BackfillSpec,
    allowed_engine_trigger: Option<&AllowedOnlineRenameTrigger>,
    managed_guard: Option<&GuardIdentity>,
) -> Result<(), ApplyError> {
    let rows = conn
        .query(
            ENABLED_USER_TRIGGER_SQL,
            &[
                Bind::Text(spec.schema.clone()),
                Bind::Text(spec.table.clone()),
            ],
        )
        .await?;
    let mut allowed_engine_trigger_seen = allowed_engine_trigger.is_none();
    for row in &rows {
        let schema: String = row.try_get("trigger_schema")?;
        let table: String = row.try_get("trigger_table")?;
        let name: String = row.try_get("trigger_name")?;
        let on_root = schema == spec.schema && table == spec.table;
        if managed_guard.is_some_and(|guard| guard.trigger_name == name) && on_root {
            continue;
        }
        if allowed_engine_trigger.is_some_and(|expected| expected.trigger_name == name) && on_root {
            prove_allowed_engine_trigger(
                row,
                spec,
                allowed_engine_trigger.expect("matched allowed online-rename trigger"),
            )?;
            allowed_engine_trigger_seen = true;
            continue;
        }
        return Err(backend_error(format!(
            "backfill target {}.{} has an unproven enabled user trigger {schema}.{table}.{name:?}; cursor guards cannot compose safely with it",
            spec.schema, spec.table
        )));
    }
    if !allowed_engine_trigger_seen {
        return Err(backend_error(
            "the expected zero-migrate online-rename trigger is missing or disabled",
        ));
    }
    Ok(())
}

async fn install_guard<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    guard: &GuardIdentity,
) -> Result<(), ApplyError> {
    ensure_guard_target_supported(conn, spec).await?;
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let schema = quote_ident(&spec.schema)?;
    let table = quote_ident(&spec.table)?;
    let function = quote_ident(&guard.function_name)?;
    let trigger = quote_ident(&guard.trigger_name)?;
    let columns = spec
        .cursor_columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Result<Vec<_>, ApplyError>>()?
        .join(", ");
    let body = guard_function_body(spec)?;
    conn.batch(&format!(
        "CREATE FUNCTION {meta}.{function}() RETURNS trigger \
         LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog \
         AS $zero_migrate_cursor_guard${body}$zero_migrate_cursor_guard$"
    ))
    .await?;
    conn.batch(&format!(
        "CREATE TRIGGER {trigger} BEFORE UPDATE OF {columns} ON {schema}.{table} \
         FOR EACH ROW EXECUTE FUNCTION {meta}.{function}()"
    ))
    .await?;
    conn.batch(&format!(
        "ALTER TABLE {schema}.{table} ENABLE ALWAYS TRIGGER {trigger}"
    ))
    .await?;
    conn.batch(&format!(
        "COMMENT ON FUNCTION {meta}.{function}() IS '{}'; \
         COMMENT ON TRIGGER {trigger} ON {schema}.{table} IS '{}'",
        guard.marker, guard.marker
    ))
    .await?;
    Ok(())
}

const VERIFY_GUARD_SQL: &str = "SELECT t.tgtype::bigint AS trigger_type, \
            t.tgenabled::text AS trigger_enabled, p.prosecdef AS security_definer, \
            (t.tgqual IS NULL) AS no_when_clause, \
            t.tgnargs::bigint AS trigger_arg_count, \
            l.lanname AS function_language, p.prosrc AS function_body, \
            COALESCE(p.proconfig, ARRAY[]::text[]) AS function_config, \
            pg_catalog.obj_description(t.oid, 'pg_trigger') AS trigger_marker, \
            pg_catalog.obj_description(p.oid, 'pg_proc') AS function_marker, \
            ARRAY( \
                SELECT a.attname::text \
                  FROM unnest(t.tgattr::smallint[]) WITH ORDINALITY AS guarded(attnum, ordinality) \
                  JOIN pg_catalog.pg_attribute a \
                    ON a.attrelid = t.tgrelid AND a.attnum = guarded.attnum \
                 ORDER BY guarded.ordinality \
            ) AS guarded_columns \
       FROM pg_catalog.pg_trigger t \
       JOIN pg_catalog.pg_class rel ON rel.oid = t.tgrelid \
       JOIN pg_catalog.pg_namespace n ON n.oid = rel.relnamespace \
       JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
       JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace \
       JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
      WHERE n.nspname = $1 AND rel.relname = $2 \
        AND t.tgname = $3 AND NOT t.tgisinternal \
        AND pn.nspname = $4 AND p.proname = $5 \
        AND p.pronargs = 0";

async fn verify_guard<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    guard: &GuardIdentity,
) -> Result<(), ApplyError> {
    let rows = conn
        .query(
            VERIFY_GUARD_SQL,
            &[
                Bind::Text(spec.schema.clone()),
                Bind::Text(spec.table.clone()),
                Bind::Text(guard.trigger_name.clone()),
                Bind::Text(cfg.pg.meta_schema.clone()),
                Bind::Text(guard.function_name.clone()),
            ],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(backend_error(
            "the journaled zero-migrate cursor guard is missing",
        ));
    };
    let trigger_type: i64 = row.try_get("trigger_type")?;
    let enabled: String = row.try_get("trigger_enabled")?;
    let security_definer: bool = row.try_get("security_definer")?;
    let no_when_clause: bool = row.try_get("no_when_clause")?;
    let trigger_arg_count: i64 = row.try_get("trigger_arg_count")?;
    let language: String = row.try_get("function_language")?;
    let body: String = row.try_get("function_body")?;
    let config: Vec<String> = row.try_get("function_config")?;
    let trigger_marker: Option<String> = row.try_get("trigger_marker")?;
    let function_marker: Option<String> = row.try_get("function_marker")?;
    let guarded_columns: Vec<String> = row.try_get("guarded_columns")?;
    let exact = trigger_type == 19
        && enabled == "A"
        && !security_definer
        && no_when_clause
        && trigger_arg_count == 0
        && language == "plpgsql"
        && body == guard_function_body(spec)?
        && config
            .iter()
            .any(|setting| setting == "search_path=pg_catalog")
        && trigger_marker.as_deref() == Some(guard.marker.as_str())
        && function_marker.as_deref() == Some(guard.marker.as_str())
        && guarded_columns == spec.cursor_columns;
    if !exact {
        return Err(backend_error(
            "the journaled zero-migrate cursor guard definition drifted; refusing resume",
        ));
    }
    Ok(())
}

async fn drop_guard<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    guard: &GuardIdentity,
) -> Result<(), ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let schema = quote_ident(&spec.schema)?;
    let table = quote_ident(&spec.table)?;
    let function = quote_ident(&guard.function_name)?;
    let trigger = quote_ident(&guard.trigger_name)?;
    conn.batch(&format!(
        "DROP TRIGGER {trigger} ON {schema}.{table}; \
         DROP FUNCTION {meta}.{function}()"
    ))
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct Progress {
    last_cursor: Option<CursorTuple>,
    end_cursor: Option<CursorTuple>,
    complete: bool,
    rows_done: u64,
    batches_done: u64,
}

fn json_encode<T: serde::Serialize>(value: &T, what: &str) -> Result<String, ApplyError> {
    serde_json::to_string(value)
        .map_err(|error| backend_error(format!("could not encode {what}: {error}")))
}

fn json_decode<T: serde::de::DeserializeOwned>(value: &str, what: &str) -> Result<T, ApplyError> {
    serde_json::from_str(value)
        .map_err(|error| backend_error(format!("invalid persisted {what}: {error}")))
}

fn cohort_bound_checksum(
    checksum: &Checksum,
    contract: &CursorContract,
    end_cursor: Option<&CursorTuple>,
) -> Result<String, ApplyError> {
    let mut hash = Sha256::new();
    hash.update(b"zero-migrate/postgres/cohort-bound/v1");
    hash.update(checksum.as_str().as_bytes());
    let contract_json = json_encode(contract, "cursor contract")?;
    hash.update((contract_json.len() as u64).to_be_bytes());
    hash.update(contract_json.as_bytes());
    match end_cursor {
        Some(end_cursor) => {
            hash.update(b"some");
            let encoded = end_cursor
                .to_json()
                .map_err(|error| backend_error(format!("could not encode end cursor: {error}")))?;
            hash.update((encoded.len() as u64).to_be_bytes());
            hash.update(encoded.as_bytes());
        }
        None => hash.update(b"none"),
    }
    Ok(hex::encode(hash.finalize()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProgressColumnSpec {
    name: &'static str,
    data_type: &'static str,
    not_null: bool,
    default_expr: Option<&'static str>,
}

const PROGRESS_COLUMNS: &[ProgressColumnSpec] = &[
    ProgressColumnSpec {
        name: "backfill_id",
        data_type: "text",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "checksum",
        data_type: "text",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "name",
        data_type: "text",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "target_schema",
        data_type: "text",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "target_table",
        data_type: "text",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "cursor_columns",
        data_type: "jsonb",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "cursor_contract",
        data_type: "jsonb",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "cursor_stability",
        data_type: "jsonb",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "last_cursor",
        data_type: "jsonb",
        not_null: false,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "end_cursor",
        data_type: "jsonb",
        not_null: false,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "cohort_bound_checksum",
        data_type: "text",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "cohort_initialized",
        data_type: "boolean",
        not_null: true,
        default_expr: Some("false"),
    },
    ProgressColumnSpec {
        name: "rows_done",
        data_type: "bigint",
        not_null: true,
        default_expr: Some("0"),
    },
    ProgressColumnSpec {
        name: "batches_done",
        data_type: "bigint",
        not_null: true,
        default_expr: Some("0"),
    },
    ProgressColumnSpec {
        name: "complete",
        data_type: "boolean",
        not_null: true,
        default_expr: Some("false"),
    },
    ProgressColumnSpec {
        name: "guard_trigger",
        data_type: "text",
        not_null: false,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "guard_function",
        data_type: "text",
        not_null: false,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "guard_marker",
        data_type: "text",
        not_null: false,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "guard_installed",
        data_type: "boolean",
        not_null: true,
        default_expr: Some("false"),
    },
    ProgressColumnSpec {
        name: "guard_cleaned",
        data_type: "boolean",
        not_null: true,
        default_expr: Some("false"),
    },
    ProgressColumnSpec {
        name: "applied_by",
        data_type: "text",
        not_null: true,
        default_expr: None,
    },
    ProgressColumnSpec {
        name: "started_at",
        data_type: "timestamp with time zone",
        not_null: true,
        default_expr: Some("now()"),
    },
    ProgressColumnSpec {
        name: "updated_at",
        data_type: "timestamp with time zone",
        not_null: true,
        default_expr: Some("now()"),
    },
];

const PROGRESS_RELATION_SQL: &str = "SELECT c.relkind::text AS relation_kind, \
            c.relpersistence::text AS persistence, c.relnatts::bigint AS attribute_count \
       FROM pg_catalog.pg_class c \
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
      WHERE n.nspname = $1 AND c.relname = 'schema_backfills'";

const PROGRESS_COLUMNS_SQL: &str = "SELECT a.attnum::bigint AS ordinal_position, \
            a.attname::text AS column_name, \
            pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type, \
            a.attnotnull AS not_null, a.attisdropped AS dropped, \
            a.attidentity::text AS identity_kind, \
            a.attgenerated::text AS generated_kind, \
            pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS default_expr \
       FROM pg_catalog.pg_attribute a \
       JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
       LEFT JOIN pg_catalog.pg_attrdef d \
         ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
      WHERE n.nspname = $1 AND c.relname = 'schema_backfills' \
        AND a.attnum > 0 \
      ORDER BY a.attnum";

const PROGRESS_CONSTRAINTS_SQL: &str = "SELECT con.contype::text AS constraint_type, \
            ARRAY(SELECT a.attname::text \
                    FROM unnest(con.conkey) WITH ORDINALITY AS key(attnum, ordinality) \
                    JOIN pg_catalog.pg_attribute a \
                      ON a.attrelid = con.conrelid AND a.attnum = key.attnum \
                   ORDER BY key.ordinality) AS constraint_columns \
       FROM pg_catalog.pg_constraint con \
       JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
      WHERE n.nspname = $1 AND c.relname = 'schema_backfills' \
        AND con.contype <> 'n' \
      ORDER BY con.oid";

fn progress_schema_error(detail: impl Into<String>) -> ApplyError {
    backend_error(format!(
        "schema_backfills does not have the exact current pre-release schema: {}",
        detail.into()
    ))
}

fn verify_progress_schema(
    relation: &Row,
    columns: &[Row],
    constraints: &[Row],
) -> Result<(), ApplyError> {
    let relation_kind: String = relation.try_get("relation_kind")?;
    let persistence: String = relation.try_get("persistence")?;
    let attribute_count: i64 = relation.try_get("attribute_count")?;
    if relation_kind != "r"
        || persistence != "p"
        || attribute_count != i64::try_from(PROGRESS_COLUMNS.len()).unwrap_or(i64::MAX)
    {
        return Err(progress_schema_error(format!(
            "expected an ordinary permanent table with {} attributes, found kind={relation_kind:?}, persistence={persistence:?}, attributes={attribute_count}",
            PROGRESS_COLUMNS.len()
        )));
    }
    if columns.len() != PROGRESS_COLUMNS.len() {
        return Err(progress_schema_error(format!(
            "expected {} ordered columns, found {}",
            PROGRESS_COLUMNS.len(),
            columns.len()
        )));
    }
    for (index, (row, expected)) in columns.iter().zip(PROGRESS_COLUMNS).enumerate() {
        let ordinal: i64 = row.try_get("ordinal_position")?;
        let name: String = row.try_get("column_name")?;
        let data_type: String = row.try_get("data_type")?;
        let not_null: bool = row.try_get("not_null")?;
        let dropped: bool = row.try_get("dropped")?;
        let identity: String = row.try_get("identity_kind")?;
        let generated: String = row.try_get("generated_kind")?;
        let default_expr: Option<String> = row.try_get("default_expr")?;
        let expected_ordinal = i64::try_from(index + 1).unwrap_or(i64::MAX);
        if ordinal != expected_ordinal
            || name != expected.name
            || data_type != expected.data_type
            || not_null != expected.not_null
            || dropped
            || !identity.is_empty()
            || !generated.is_empty()
            || default_expr.as_deref() != expected.default_expr
        {
            return Err(progress_schema_error(format!(
                "column {} differs: ordinal={ordinal}, name={name:?}, type={data_type:?}, not_null={not_null}, dropped={dropped}, identity={identity:?}, generated={generated:?}, default={default_expr:?}",
                index + 1
            )));
        }
    }
    if constraints.len() != 1 {
        return Err(progress_schema_error(format!(
            "expected only the backfill_id primary key constraint, found {} constraints",
            constraints.len()
        )));
    }
    let constraint_type: String = constraints[0].try_get("constraint_type")?;
    let constraint_columns: Vec<String> = constraints[0].try_get("constraint_columns")?;
    if constraint_type != "p" || constraint_columns != ["backfill_id"] {
        return Err(progress_schema_error(format!(
            "expected PRIMARY KEY (backfill_id), found type={constraint_type:?}, columns={constraint_columns:?}"
        )));
    }
    Ok(())
}

async fn ensure_progress<D: SqlSession>(conn: &D, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    conn.batch(&format!("CREATE SCHEMA IF NOT EXISTS {meta}"))
        .await?;
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_backfills (\
            backfill_id TEXT PRIMARY KEY, \
            checksum TEXT NOT NULL, \
            name TEXT NOT NULL, \
            target_schema TEXT NOT NULL, \
            target_table TEXT NOT NULL, \
            cursor_columns JSONB NOT NULL, \
            cursor_contract JSONB NOT NULL, \
            cursor_stability JSONB NOT NULL, \
            last_cursor JSONB, \
            end_cursor JSONB, \
            cohort_bound_checksum TEXT NOT NULL, \
            cohort_initialized BOOLEAN NOT NULL DEFAULT false, \
            rows_done BIGINT NOT NULL DEFAULT 0, \
            batches_done BIGINT NOT NULL DEFAULT 0, \
            complete BOOLEAN NOT NULL DEFAULT false, \
            guard_trigger TEXT, \
            guard_function TEXT, \
            guard_marker TEXT, \
            guard_installed BOOLEAN NOT NULL DEFAULT false, \
            guard_cleaned BOOLEAN NOT NULL DEFAULT false, \
            applied_by TEXT NOT NULL, \
            started_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()\
        )"
    ))
    .await?;

    let bind = [Bind::Text(cfg.pg.meta_schema.clone())];
    let relation = conn.query_one(PROGRESS_RELATION_SQL, &bind).await?;
    let columns = conn.query(PROGRESS_COLUMNS_SQL, &bind).await?;
    let constraints = conn.query(PROGRESS_CONSTRAINTS_SQL, &bind).await?;
    verify_progress_schema(&relation, &columns, &constraints)
}

async fn read_progress_row<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    for_update: bool,
) -> Result<Option<Row>, ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let lock = if for_update { " FOR UPDATE" } else { "" };
    let rows = conn
        .query(
            &format!(
                "SELECT checksum, target_schema, target_table, \
                        cursor_columns::text AS cursor_columns_json, \
                        cursor_contract::text AS cursor_contract_json, \
                        cursor_stability::text AS cursor_stability_json, \
                        last_cursor::text AS last_cursor_json, \
                        end_cursor::text AS end_cursor_json, \
                        cohort_bound_checksum, cohort_initialized, \
                        rows_done, batches_done, complete, \
                        guard_trigger, guard_function, guard_marker, \
                        guard_installed, guard_cleaned \
                   FROM {meta}.schema_backfills \
                  WHERE backfill_id = $1{lock}"
            ),
            &[Bind::Text(backfill_id.to_string())],
        )
        .await?;
    Ok(rows.into_iter().next())
}

fn decode_progress(
    row: &Row,
    spec: &BackfillSpec,
    checksum: &Checksum,
    contract: &CursorContract,
    expected_guard: Option<&GuardIdentity>,
) -> Result<Progress, ApplyError> {
    let recorded_checksum: String = row.try_get("checksum")?;
    if recorded_checksum != checksum.as_str() {
        return Err(ApplyError::ChecksumDrift {
            version: spec.name.clone(),
            recorded: recorded_checksum,
            expected: checksum.as_str().to_string(),
        });
    }
    let target_schema: String = row.try_get("target_schema")?;
    let target_table: String = row.try_get("target_table")?;
    if target_schema != spec.schema || target_table != spec.table {
        return Err(backend_error(format!(
            "progress target drifted from {}.{} to {target_schema}.{target_table}",
            spec.schema, spec.table
        )));
    }

    let cursor_columns_json: String = row.try_get("cursor_columns_json")?;
    let cursor_columns: Vec<String> = json_decode(&cursor_columns_json, "cursorColumns")?;
    if cursor_columns != spec.cursor_columns {
        return Err(backend_error(format!(
            "progress cursorColumns drifted: recorded {cursor_columns:?}, expected {:?}",
            spec.cursor_columns
        )));
    }
    let contract_json: String = row.try_get("cursor_contract_json")?;
    let recorded_contract: CursorContract = json_decode(&contract_json, "cursor contract")?;
    if recorded_contract != *contract {
        return Err(backend_error(format!(
            "progress cursor type/collation contract drifted: recorded {recorded_contract:?}, expected {contract:?}"
        )));
    }
    let stability_json: String = row.try_get("cursor_stability_json")?;
    let stability: CursorStability = json_decode(&stability_json, "cursor stability")?;
    if stability != spec.cursor_stability {
        return Err(backend_error(format!(
            "progress cursorStability drifted: recorded {stability:?}, expected {:?}",
            spec.cursor_stability
        )));
    }

    let last_cursor_json: Option<String> = row.try_get("last_cursor_json")?;
    let last_cursor = last_cursor_json
        .as_deref()
        .map(|value| CursorTuple::from_json(value, contract))
        .transpose()
        .map_err(|error| backend_error(format!("invalid lastCursor checkpoint: {error}")))?;
    let end_cursor_json: Option<String> = row.try_get("end_cursor_json")?;
    let end_cursor = end_cursor_json
        .as_deref()
        .map(|value| CursorTuple::from_json(value, contract))
        .transpose()
        .map_err(|error| backend_error(format!("invalid endCursor checkpoint: {error}")))?;
    let recorded_bound: String = row.try_get("cohort_bound_checksum")?;
    let expected_bound = cohort_bound_checksum(checksum, contract, end_cursor.as_ref())?;
    if recorded_bound != expected_bound {
        return Err(backend_error(
            "persisted endCursor failed its cohort-bound integrity checksum",
        ));
    }

    let guard_trigger: Option<String> = row.try_get("guard_trigger")?;
    let guard_function: Option<String> = row.try_get("guard_function")?;
    let guard_marker: Option<String> = row.try_get("guard_marker")?;
    let guard = match (guard_trigger, guard_function, guard_marker) {
        (Some(trigger_name), Some(function_name), Some(marker)) => Some(GuardIdentity {
            trigger_name,
            function_name,
            marker,
        }),
        (None, None, None) => None,
        _ => {
            return Err(backend_error(
                "progress contains a partial cursor-guard obligation",
            ));
        }
    };
    if guard.as_ref() != expected_guard {
        return Err(backend_error(format!(
            "progress cursor-guard identity drifted: recorded {guard:?}, expected {expected_guard:?}"
        )));
    }
    let guard_installed: bool = row.try_get("guard_installed")?;
    let guard_cleaned: bool = row.try_get("guard_cleaned")?;
    let complete: bool = row.try_get("complete")?;
    match (&spec.cursor_stability, complete) {
        (CursorStability::GuardUpdates, false) if !guard_installed || guard_cleaned => {
            return Err(backend_error(
                "incomplete progress lost its installed cursor-guard obligation",
            ));
        }
        (CursorStability::GuardUpdates, true) if guard_installed || !guard_cleaned => {
            return Err(backend_error(
                "completed progress did not journal cursor-guard cleanup",
            ));
        }
        (CursorStability::ExternalInvariant { .. }, _)
            if guard.is_some() || guard_installed || !guard_cleaned =>
        {
            return Err(backend_error(
                "externalInvariant progress unexpectedly carries a database guard",
            ));
        }
        _ => {}
    }

    let rows_done_i64: i64 = row.try_get("rows_done")?;
    let batches_done_i64: i64 = row.try_get("batches_done")?;
    let rows_done = u64::try_from(rows_done_i64)
        .map_err(|_| backend_error("progress rows_done is negative"))?;
    let batches_done = u64::try_from(batches_done_i64)
        .map_err(|_| backend_error("progress batches_done is negative"))?;
    let cohort_initialized: bool = row.try_get("cohort_initialized")?;
    if !cohort_initialized {
        return Err(backend_error("progress row has no durable cohort boundary"));
    }
    if end_cursor.is_none() && last_cursor.is_some() {
        return Err(backend_error(
            "progress records lastCursor for an empty bounded cohort",
        ));
    }
    Ok(Progress {
        last_cursor,
        end_cursor,
        complete,
        rows_done,
        batches_done,
    })
}

pub(super) async fn read_progress_entries<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<BackfillProgressEntry>, JournalError> {
    let catalog = conn
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = 'schema_backfills' \
                   AND c.relkind IN ('r', 'p') \
             ) AS table_exists",
            &[Bind::Text(cfg.pg.meta_schema.clone())],
        )
        .await?;
    let table_exists: bool = catalog.try_get("table_exists")?;
    if !table_exists {
        return Ok(Vec::new());
    }
    let meta = crate::render::dml::quote_ident_checked(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "SELECT backfill_id, checksum, complete \
                   FROM {meta}.schema_backfills"
            ),
            &[],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(BackfillProgressEntry {
                version: row.try_get("backfill_id")?,
                checksum: Some(row.try_get("checksum")?),
                complete: row.try_get("complete")?,
            })
        })
        .collect()
}

/// The per-batch session envelope.
///
/// Both budgets go through [`resolve_timeout_ms`] rather than being interpolated
/// from the config getters directly. A backfill step carries no per-migration
/// override, so the values come from the executor config either way - but
/// `ExecutorConfig::statement_timeout_ms` is an `as_millis()` truncation, so a
/// sub-millisecond `Duration` arrives here as `0`, and `SET LOCAL
/// statement_timeout = 0` is how PostgreSQL spells NO LIMIT. Interpolating turned
/// a config meaning "fail fast" into "wait forever" on this path alone: every
/// sibling render already asks the resolver, including the MySQL backfill.
fn batch_session_sql(
    cfg: &ExecutorConfig,
    version: &MigrationId,
    spec: &BackfillSpec,
) -> Result<String, ApplyError> {
    Ok(format!(
        "{AUTHOR_SQL_LITERAL_MODE} \
         {CURSOR_SESSION_SETTINGS} \
         SET LOCAL search_path TO {}; \
         SET LOCAL statement_timeout = {}; \
         SET LOCAL lock_timeout = {};",
        quote_ident(&spec.schema)?,
        resolve_timeout_ms(
            version.as_str(),
            "statement_timeout",
            None,
            "timeout_ms",
            cfg.statement_timeout_ms(),
            "pg.statement_timeout",
        )?,
        resolve_timeout_ms(
            version.as_str(),
            "lock_timeout",
            None,
            "lock_timeout_ms",
            cfg.lock_timeout_ms(),
            "pg.lock_timeout",
        )?,
    ))
}

async fn rollback<D: SqlSession>(conn: &D) {
    if let Err(error) = conn.batch("ROLLBACK").await {
        tracing::warn!(error = %error, "zero-migrate: PostgreSQL backfill rollback failed");
    }
}

async fn validate_target_under_lock<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    expected: &PgCursor,
    allowed_engine_trigger: Option<&AllowedOnlineRenameTrigger>,
    managed_guard: Option<&GuardIdentity>,
) -> Result<(), ApplyError> {
    let target = format!(
        "{}.{}",
        quote_ident(&spec.schema)?,
        quote_ident(&spec.table)?
    );
    conn.batch(&format!("LOCK TABLE {target} IN ROW EXCLUSIVE MODE"))
        .await?;
    let live = inspect_cursor(conn, spec).await?;
    if live != *expected {
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "cursor type/collation/cast contract changed while running: expected {expected:?}, live {live:?}"
            ),
        ));
    }
    reject_trigger_interactions(conn, spec, allowed_engine_trigger, managed_guard).await?;
    if let Some(guard) = managed_guard {
        verify_guard(conn, cfg, spec, guard).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn initialize_progress<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    cursor: &PgCursor,
    guard: Option<&GuardIdentity>,
    allowed_engine_trigger: Option<&AllowedOnlineRenameTrigger>,
    applied_by: &str,
) -> Result<Progress, ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    conn.batch("BEGIN").await?;
    let result = async {
        conn.batch(&batch_session_sql(cfg, version, spec)?).await?;
        validate_target_under_lock(conn, cfg, spec, cursor, allowed_engine_trigger, None).await?;

        if let Some(guard) = guard {
            install_guard(conn, cfg, spec, guard).await?;
            reject_trigger_interactions(conn, spec, allowed_engine_trigger, Some(guard)).await?;
            verify_guard(conn, cfg, spec, guard).await?;
        }

        if let Some(role) = &cfg.pg.migrator_role {
            conn.batch(&format!("SET LOCAL ROLE {}", quote_ident(role)?))
                .await?;
        }
        let end_rows = conn
            .query(&build_end_cursor_sql(spec, cursor)?, &[])
            .await?;
        let end_cursor = end_rows
            .first()
            .map(|row| tuple_from_row(row, "_bf_end_", cursor))
            .transpose()?;
        if cfg.pg.migrator_role.is_some() {
            conn.batch("RESET ROLE").await?;
        }

        let cursor_columns_json = json_encode(&spec.cursor_columns, "cursorColumns")?;
        let contract_json = json_encode(&cursor.contract, "cursor contract")?;
        let stability_json = json_encode(&spec.cursor_stability, "cursor stability")?;
        let end_cursor_json = end_cursor
            .as_ref()
            .map(CursorTuple::to_json)
            .transpose()
            .map_err(|error| backend_error(format!("could not encode endCursor: {error}")))?;
        let bound_checksum =
            cohort_bound_checksum(checksum, &cursor.contract, end_cursor.as_ref())?;
        let guard_installed = guard.is_some();
        let guard_cleaned = guard.is_none();
        let inserted = conn
            .exec(
                &format!(
                    "INSERT INTO {meta}.schema_backfills (\
                        backfill_id, checksum, name, target_schema, target_table, \
                        cursor_columns, cursor_contract, cursor_stability, \
                        end_cursor, cohort_bound_checksum, cohort_initialized, \
                        guard_trigger, guard_function, guard_marker, \
                        guard_installed, guard_cleaned, applied_by\
                     ) VALUES (\
                        $1, $2, $3, $4, $5, \
                        $6::text::jsonb, $7::text::jsonb, $8::text::jsonb, \
                        $9::text::jsonb, $10, true, \
                        $11, $12, $13, $14, $15, $16\
                     )"
                ),
                &[
                    Bind::Text(version.as_str().to_string()),
                    Bind::Text(checksum.as_str().to_string()),
                    Bind::Text(spec.name.clone()),
                    Bind::Text(spec.schema.clone()),
                    Bind::Text(spec.table.clone()),
                    Bind::Text(cursor_columns_json),
                    Bind::Text(contract_json),
                    Bind::Text(stability_json),
                    end_cursor_json.into(),
                    Bind::Text(bound_checksum),
                    guard.map_or(Bind::Null, |guard| Bind::Text(guard.trigger_name.clone())),
                    guard.map_or(Bind::Null, |guard| Bind::Text(guard.function_name.clone())),
                    guard.map_or(Bind::Null, |guard| Bind::Text(guard.marker.clone())),
                    Bind::Bool(guard_installed),
                    Bind::Bool(guard_cleaned),
                    Bind::Text(applied_by.to_string()),
                ],
            )
            .await?;
        if inserted != 1 {
            return Err(backend_error(format!(
                "progress initialization affected {inserted} rows for {:?}",
                version.as_str()
            )));
        }
        Ok::<Progress, ApplyError>(Progress {
            last_cursor: None,
            end_cursor,
            complete: false,
            rows_done: 0,
            batches_done: 0,
        })
    }
    .await;
    match result {
        Ok(progress) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(conn).await;
                return Err(error.into());
            }
            Ok(progress)
        }
        Err(error) => {
            rollback(conn).await;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn lock_and_validate_progress<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    cursor: &PgCursor,
    guard: Option<&GuardIdentity>,
    expected_last_cursor: Option<&CursorTuple>,
    expected_end_cursor: &CursorTuple,
) -> Result<Progress, ApplyError> {
    let row = read_progress_row(conn, cfg, version.as_str(), true)
        .await?
        .ok_or_else(|| {
            backend_error(format!(
                "progress row disappeared for {:?}",
                version.as_str()
            ))
        })?;
    let progress = decode_progress(&row, spec, checksum, &cursor.contract, guard)?;
    if progress.complete {
        return Err(backend_error("progress became complete during a batch"));
    }
    if progress.last_cursor.as_ref() != expected_last_cursor
        || progress.end_cursor.as_ref() != Some(expected_end_cursor)
    {
        return Err(backend_error(format!(
            "progress cursor changed concurrently: recorded last={:?}, end={:?}; expected last={expected_last_cursor:?}, end={expected_end_cursor:?}",
            progress.last_cursor, progress.end_cursor
        )));
    }
    Ok(progress)
}

#[allow(clippy::too_many_arguments)]
async fn run_batch<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    cursor: &PgCursor,
    guard: Option<&GuardIdentity>,
    allowed_engine_trigger: Option<&AllowedOnlineRenameTrigger>,
    last_cursor: Option<&CursorTuple>,
    end_cursor: &CursorTuple,
) -> Result<(u64, u64, Option<CursorTuple>), ApplyError> {
    conn.batch("BEGIN").await?;
    let result = async {
        conn.batch(&batch_session_sql(cfg, version, spec)?).await?;
        lock_and_validate_progress(
            conn,
            cfg,
            version,
            checksum,
            spec,
            cursor,
            guard,
            last_cursor,
            end_cursor,
        )
        .await?;
        validate_target_under_lock(
            conn,
            cfg,
            spec,
            cursor,
            allowed_engine_trigger,
            guard,
        )
        .await?;

        if let Some(role) = &cfg.pg.migrator_role {
            conn.batch(&format!("SET LOCAL ROLE {}", quote_ident(role)?))
                .await?;
        }
        let binds = batch_binds(last_cursor, end_cursor);
        let (selected, updated, next_cursor) = if spec.per_row.is_empty() {
            let sql = build_batch_sql(spec, cursor, last_cursor.is_some())?;
            let row = conn.query_one(&sql, &binds).await.map_err(|error| {
                backend_error(format!(
                    "batch failed after cursor {last_cursor:?}: {error}"
                ))
            })?;
            let selected_i64: i64 = row.try_get("_bf_selected")?;
            let selected = u64::try_from(selected_i64)
                .map_err(|_| backend_error("database returned a negative selected-row count"))?;
            let updated_i64: i64 = row.try_get("_bf_rows")?;
            let updated = u64::try_from(updated_i64)
                .map_err(|_| backend_error("database returned a negative updated-row count"))?;
            let next_cursor = if selected == 0 {
                None
            } else {
                Some(tuple_from_row(&row, "_bf_cursor_", cursor)?)
            };
            (selected, updated, next_cursor)
        } else {
            let window_sql =
                build_per_row_window_sql(spec, cursor, last_cursor.is_some())?;
            let selected_rows = conn.query(&window_sql, &binds).await.map_err(|error| {
                backend_error(format!(
                    "batch window failed after cursor {last_cursor:?}: {error}"
                ))
            })?;
            let selected_tuples = selected_rows
                .iter()
                .map(|row| tuple_from_row(row, "_bf_cursor_", cursor))
                .collect::<Result<Vec<_>, ApplyError>>()?;
            let update_sql = build_per_row_update_sql(spec, cursor)?;
            let mut updated = 0_u64;
            for selected_tuple in &selected_tuples {
                let mut parameters = spec
                    .per_row
                    .values()
                    .map(|assignment| generate_per_row_value(assignment.generator()))
                    .map(Some)
                    .collect::<Vec<_>>();
                parameters.extend(tuple_binds(selected_tuple).into_iter().map(|bind| match bind {
                    Bind::Text(value) => Some(value),
                    _ => unreachable!("cursor tuple binds are text"),
                }));
                let affected = conn.exec_text(&update_sql, &parameters).await.map_err(|error| {
                    backend_error(format!(
                        "per-row update failed at cursor {selected_tuple:?}: {error}"
                    ))
                })?;
                if affected != 1 {
                    return Err(backend_error(format!(
                        "per-row update at cursor {selected_tuple:?} affected {affected} rows; expected exactly one"
                    )));
                }
                updated = updated.saturating_add(affected);
            }
            let selected = u64::try_from(selected_tuples.len())
                .map_err(|_| backend_error("selected-row count exceeds u64"))?;
            let next_cursor = selected_tuples.last().cloned();
            (selected, updated, next_cursor)
        };
        if cfg.pg.migrator_role.is_some() {
            conn.batch("RESET ROLE").await?;
        }

        if selected != updated {
            return Err(backend_error(format!(
                "batch selected {selected} rows but updated {updated}; a trigger or asymmetric row-level security policy suppressed target rows"
            )));
        }
        if selected > 0 {
            let next_cursor = next_cursor
                .as_ref()
                .ok_or_else(|| backend_error("non-empty batch returned no cursor tuple"))?;
            let next_json = next_cursor
                .to_json()
                .map_err(|error| backend_error(format!("could not encode checkpoint: {error}")))?;
            let rows_i64 = i64::try_from(updated)
                .map_err(|_| backend_error("updated-row count cannot be checkpointed"))?;
            let meta = quote_ident(&cfg.pg.meta_schema)?;
            let advanced = conn
                .exec(
                    &format!(
                        "UPDATE {meta}.schema_backfills \
                            SET last_cursor = $3::text::jsonb, rows_done = rows_done + $4, \
                                batches_done = batches_done + 1, updated_at = now() \
                          WHERE backfill_id = $1 AND checksum = $2 AND complete = false"
                    ),
                    &[
                        Bind::Text(version.as_str().to_string()),
                        Bind::Text(checksum.as_str().to_string()),
                        Bind::Text(next_json),
                        Bind::Int(rows_i64),
                    ],
                )
                .await?;
            if advanced != 1 {
                return Err(backend_error(format!(
                    "progress update affected {advanced} rows for {:?}",
                    version.as_str()
                )));
            }
        }
        Ok::<(u64, u64, Option<CursorTuple>), ApplyError>((
            selected,
            updated,
            next_cursor,
        ))
    }
    .await;
    match result {
        Ok(outcome) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(conn).await;
                return Err(error.into());
            }
            Ok(outcome)
        }
        Err(error) => {
            rollback(conn).await;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_backfill<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    cursor: &PgCursor,
    guard: Option<&GuardIdentity>,
    allowed_engine_trigger: Option<&AllowedOnlineRenameTrigger>,
    applied_by: &str,
) -> Result<(), ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    conn.batch("BEGIN").await?;
    let result = async {
        conn.batch(&batch_session_sql(cfg, version, spec)?).await?;
        let row = read_progress_row(conn, cfg, version.as_str(), true)
            .await?
            .ok_or_else(|| backend_error("progress row disappeared during completion"))?;
        let progress = decode_progress(&row, spec, checksum, &cursor.contract, guard)?;

        if !progress.complete {
            validate_target_under_lock(
                conn,
                cfg,
                spec,
                cursor,
                allowed_engine_trigger,
                guard,
            )
            .await?;
            if let Some(guard) = guard {
                drop_guard(conn, cfg, spec, guard).await?;
            }
            let completed = conn
                .exec(
                    &format!(
                        "UPDATE {meta}.schema_backfills \
                            SET complete = true, guard_installed = false, \
                                guard_cleaned = true, updated_at = now() \
                          WHERE backfill_id = $1 AND checksum = $2 \
                            AND cursor_columns = $3::text::jsonb \
                            AND cursor_contract = $4::text::jsonb \
                            AND cursor_stability = $5::text::jsonb"
                    ),
                    &[
                        Bind::Text(version.as_str().to_string()),
                        Bind::Text(checksum.as_str().to_string()),
                        Bind::Text(json_encode(&spec.cursor_columns, "cursorColumns")?),
                        Bind::Text(json_encode(&cursor.contract, "cursor contract")?),
                        Bind::Text(json_encode(&spec.cursor_stability, "cursor stability")?),
                    ],
                )
                .await?;
            if completed != 1 {
                return Err(backend_error(format!(
                    "completion update affected {completed} rows for {:?}",
                    version.as_str()
                )));
            }
        }

        let latest = conn
            .query(
                &format!(
                    "SELECT event_kind, checksum FROM {meta}.schema_migrations \
                      WHERE version = $1 ORDER BY event_seq DESC LIMIT 1"
                ),
                &[Bind::Text(version.as_str().to_string())],
            )
            .await?;
        let already_journaled = if let Some(row) = latest.first() {
            let event_kind: String = row.try_get("event_kind")?;
            if event_kind == journal::EventKind::Applied.as_str() {
                let recorded: String = row.try_get("checksum")?;
                if recorded != checksum.as_str() {
                    return Err(ApplyError::ChecksumDrift {
                        version: version.as_str().to_string(),
                        recorded,
                        expected: checksum.as_str().to_string(),
                    });
                }
                true
            } else {
                false
            }
        } else {
            false
        };
        if !already_journaled {
            conn.exec(
                &format!(
                    "INSERT INTO {meta}.schema_migrations \
                        (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind) \
                     VALUES ('{applied}', $1, $2, $3, $4, 0, \
                             'completed', 'success', 'apply')",
                    applied = journal::EventKind::Applied.as_str()
                ),
                &[
                    Bind::Text(version.as_str().to_string()),
                    Bind::Text(spec.name.clone()),
                    Bind::Text(checksum.as_str().to_string()),
                    Bind::Text(applied_by.to_string()),
                ],
            )
            .await?;
        }
        Ok::<(), ApplyError>(())
    }
    .await;
    match result {
        Ok(()) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(conn).await;
                return Err(error.into());
            }
            Ok(())
        }
        Err(error) => {
            rollback(conn).await;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_backfill<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    approval: Approval,
    allowed_engine_trigger: Option<&AllowedOnlineRenameTrigger>,
    applied_by: &str,
) -> Result<BackfillOutcome, ApplyError> {
    if approval != Approval::Approved {
        return Err(ApplyError::ApprovalRequired);
    }
    validate_spec(spec)?;
    // An `externalInvariant` cursor is not a warning: it is the approved shape of
    // the spec, restated on every single backfill invocation. The plan status
    // manifest already carries it as `cursorStabilityMode` +
    // `cursorStabilityInvariant`, machine-readable, BEFORE the operator approves --
    // which is where the decision is actually made. The MySQL backend states the
    // same spec and warns about nothing. Every other event in this tree is a
    // secondary failure the reply could not carry; this one is expected state that
    // the reply already carries.

    ensure_progress(conn, cfg).await?;
    let cursor = inspect_cursor(conn, spec).await?;
    let managed_guard = matches!(spec.cursor_stability, CursorStability::GuardUpdates)
        .then(|| guard_identity(version, checksum, spec));
    let existing = read_progress_row(conn, cfg, version.as_str(), false).await?;
    let mut progress = existing
        .as_ref()
        .map(|row| {
            decode_progress(
                row,
                spec,
                checksum,
                &cursor.contract,
                managed_guard.as_ref(),
            )
        })
        .transpose()?;
    let resumed = progress.as_ref().is_some_and(|progress| {
        progress.last_cursor.is_some() || progress.rows_done > 0 || progress.batches_done > 0
    });
    if progress.as_ref().is_some_and(|progress| progress.complete) {
        finish_backfill(
            conn,
            cfg,
            version,
            checksum,
            spec,
            &cursor,
            managed_guard.as_ref(),
            allowed_engine_trigger,
            applied_by,
        )
        .await?;
        return Ok(BackfillOutcome {
            backfill_id: version.as_str().to_string(),
            batches: 0,
            rows_updated: 0,
            resumed,
            complete: true,
        });
    }

    let guard = SqlGuard::new(cfg.guard_config());
    let end_sql = build_end_cursor_sql(spec, &cursor)?;
    guard
        .check(&end_sql)
        .map_err(|source| backend_error(BackfillError::Guard { source }.to_string()))?;
    for have_cursor in [false, true] {
        let sql = if spec.per_row.is_empty() {
            build_batch_sql(spec, &cursor, have_cursor)?
        } else {
            build_per_row_window_sql(spec, &cursor, have_cursor)?
        };
        guard
            .check(&sql)
            .map_err(|source| backend_error(BackfillError::Guard { source }.to_string()))?;
        if spec.per_row.is_empty() {
            assert_cursor_not_mutated(&sql, &spec.cursor_columns)?;
        }
    }
    if !spec.per_row.is_empty() {
        let update_sql = build_per_row_update_sql(spec, &cursor)?;
        guard
            .check(&update_sql)
            .map_err(|source| backend_error(BackfillError::Guard { source }.to_string()))?;
        assert_cursor_not_mutated(&update_sql, &spec.cursor_columns)?;
    }

    if progress.is_none() {
        progress = Some(
            initialize_progress(
                conn,
                cfg,
                version,
                checksum,
                spec,
                &cursor,
                managed_guard.as_ref(),
                allowed_engine_trigger,
                applied_by,
            )
            .await?,
        );
    }
    let progress = progress.expect("progress initialized");
    let mut last_cursor = progress.last_cursor;
    let end_cursor = progress.end_cursor;
    let mut batches = 0_u64;
    let mut rows_updated = 0_u64;

    if let Some(end_cursor) = end_cursor.as_ref() {
        loop {
            let (selected, updated, next_cursor) = run_batch(
                conn,
                cfg,
                version,
                checksum,
                spec,
                &cursor,
                managed_guard.as_ref(),
                allowed_engine_trigger,
                last_cursor.as_ref(),
                end_cursor,
            )
            .await?;
            if selected == 0 {
                break;
            }
            batches = batches.saturating_add(1);
            rows_updated = rows_updated.saturating_add(updated);
            last_cursor = next_cursor;
            crate::fault::trip(crate::fault::points::BACKFILL_MID_BATCHES)?;
            if selected < u64::from(spec.batch_size) {
                break;
            }
        }
    }

    finish_backfill(
        conn,
        cfg,
        version,
        checksum,
        spec,
        &cursor,
        managed_guard.as_ref(),
        allowed_engine_trigger,
        applied_by,
    )
    .await?;
    Ok(BackfillOutcome {
        backfill_id: version.as_str().to_string(),
        batches,
        rows_updated,
        resumed,
        complete: true,
    })
}

#[cfg(test)]
mod tuple_tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::driver::Value as DriverValue;

    fn contract() -> CursorContract {
        CursorContract {
            columns: vec![
                CursorColumnContract {
                    name: "tenant_id".into(),
                    scalar_type: CursorScalarType::Int64,
                    database_type: "bigint".into(),
                    comparison: CursorComparison::Default,
                },
                CursorColumnContract {
                    name: "id".into(),
                    scalar_type: CursorScalarType::String,
                    database_type: "text".into(),
                    comparison: CursorComparison::NamedCollation {
                        schema: Some("pg_catalog".into()),
                        name: "C".into(),
                    },
                },
            ],
        }
    }

    fn spec() -> BackfillSpec {
        BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_columns: vec!["tenant_id".into(), "id".into()],
            cursor_stability: CursorStability::GuardUpdates,
            cursor_contract: Some(contract()),
            batch_size: 250,
            set_clause: "\"display_name\" = \"name\"".into(),
            per_row: BTreeMap::new(),
            filter: Some("\"display_name\" IS NULL".into()),
            name: "fill_display_name".into(),
        }
    }

    fn checksum() -> Checksum {
        Checksum::of(&crate::model::migration::ChecksumInput {
            up: "postgres tuple backfill test",
            down: None,
            flags: &crate::model::migration::MigrationFlags::default(),
            owner_app: "app",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        })
    }

    #[test]
    fn composite_window_is_lexicographic_and_bounded_in_declared_order() {
        let spec = spec();
        let cursor = PgCursor::from_contract(contract()).expect("cursor renders");
        let sql = build_batch_sql(&spec, &cursor, true).expect("batch SQL renders");
        assert!(sql.contains(
            "(_bf_source.\"tenant_id\" > ($1::text)::bigint) OR (_bf_source.\"tenant_id\" = ($1::text)::bigint AND _bf_source.\"id\" COLLATE \"pg_catalog\".\"C\" > ($2::text)::text COLLATE \"pg_catalog\".\"C\")"
        ));
        assert!(sql.contains("NOT ((_bf_source.\"tenant_id\" > ($3::text)::bigint) OR"));
        assert!(sql.contains(
            "ORDER BY _bf_source.\"tenant_id\" ASC, _bf_source.\"id\" COLLATE \"pg_catalog\".\"C\" ASC"
        ));
        assert!(sql.contains("LIMIT 250"));
        assert!(!sql.contains("FOR UPDATE"));
        assert!(sql.contains("AND (\"display_name\" IS NULL)"));
    }

    #[test]
    fn first_window_binds_only_the_typed_end_tuple() {
        let cursor = PgCursor::from_contract(contract()).expect("cursor renders");
        let sql = build_per_row_window_sql(&spec(), &cursor, false).unwrap();
        assert!(sql.contains("WHERE TRUE AND NOT"));
        assert!(sql.contains("($1::text)::bigint"));
        assert!(sql.contains("($2::text)::text"));
        assert!(!sql.contains("$3"));

        let end = CursorTuple::new(
            vec![IrScalar::Int64(9), IrScalar::Str("z|embedded".into())],
            &contract(),
        )
        .unwrap();
        assert_eq!(
            batch_binds(None, &end),
            vec![Bind::Text("9".into()), Bind::Text("z|embedded".into())]
        );
        assert_eq!(end.to_json().unwrap(), r#"[{"int64":"9"},"z|embedded"]"#);
    }

    #[test]
    fn terminal_tuple_uses_every_component_and_the_authored_filter() {
        let cursor = PgCursor::from_contract(contract()).expect("cursor renders");
        let sql = build_end_cursor_sql(&spec(), &cursor).unwrap();
        assert!(sql.contains("\"tenant_id\"::text AS _bf_end_0"));
        assert!(sql.contains("\"id\" COLLATE \"pg_catalog\".\"C\"::text AS _bf_end_1"));
        assert!(
            sql.contains("ORDER BY \"tenant_id\" DESC, \"id\" COLLATE \"pg_catalog\".\"C\" DESC")
        );
    }

    /// A cursor projection cast to text MUST alias away from the key it orders by.
    ///
    /// PostgreSQL names a bare cast expression after its underlying column, and
    /// `ORDER BY` resolves an OUTPUT-column name in preference to an input one. So
    /// `SELECT _bf_key_0::text FROM _bf_window ORDER BY _bf_key_0 DESC` sorted by
    /// TEXT, and each batch checkpointed at the text-maximum of its window. A
    /// window holding 9 and 10 checkpointed at `'9'`, so the cursor moved
    /// BACKWARDS and the next batch re-applied row 10 - a silent double
    /// application of a non-idempotent transform, with the migration reporting
    /// success and the journal recording a completed backfill.
    ///
    /// This asserts the SHAPE, at the layer the defect lived in, so it fails on
    /// generated SQL without needing a database. The live behavioural proof is
    /// `backfill-cursor-ordering.test.ts` in the host suite; this is the cheap
    /// guard that catches the alias being dropped again.
    ///
    /// `build_end_cursor_sql` above never had the defect because it already
    /// aliased its cast, which is what the test above happens to pin.
    #[test]
    fn a_text_cast_cursor_projection_never_shadows_the_key_it_orders_by() {
        let cursor = PgCursor::from_contract(contract()).expect("cursor renders");
        let sql = build_batch_sql(&spec(), &cursor, true).unwrap();

        for index in 0..cursor.arity() {
            assert!(
                !sql.contains(&format!("_bf_key_{index}::text FROM")),
                "component {index} casts to text with no alias, so its output column \
                 shadows _bf_key_{index} and ORDER BY sorts by text: {sql}"
            );
            assert!(
                sql.contains(&format!("_bf_key_{index}::text AS _bf_cursor_text_{index}")),
                "component {index} must alias its text cast away from the key name: {sql}"
            );
        }

        // The ordering must still name the KEYS. Ordering by the text alias would
        // reintroduce the identical defect by a different route.
        assert!(
            sql.contains("ORDER BY _bf_key_0 DESC, _bf_key_1 DESC"),
            "the checkpoint must order by the typed keys: {sql}"
        );
    }

    #[test]
    fn assignment_to_either_cursor_component_is_rejected_exactly() {
        let cursor = PgCursor::from_contract(contract()).expect("cursor renders");
        for component in ["tenant_id", "id"] {
            let mut value = spec();
            value.set_clause = format!("\"{component}\" = \"{component}\"");
            let sql = build_batch_sql(&value, &cursor, true).unwrap();
            let error = assert_cursor_not_mutated(&sql, &value.cursor_columns)
                .expect_err("cursor assignment must fail");
            assert!(error.to_string().contains(component));
        }

        let mut case_distinct = spec();
        case_distinct.set_clause = "\"ID\" = 1".into();
        let sql = build_batch_sql(&case_distinct, &cursor, true).unwrap();
        assert_cursor_not_mutated(&sql, &case_distinct.cursor_columns)
            .expect("quoted PostgreSQL identifiers are case-sensitive");
    }

    #[test]
    fn catalog_proof_requires_exact_default_unique_btree_semantics() {
        for required in [
            "i.indisprimary OR i.indisunique",
            "i.indisvalid AND i.indisready",
            "i.indpred IS NULL AND i.indexprs IS NULL",
            "am.amname = 'btree'",
            "i.indnkeyatts::bigint = $3::bigint",
            "opc.opcdefault",
            "i.indcollation",
            "ORDER BY key.ordinality",
        ] {
            assert!(CANDIDATE_KEY_SQL.contains(required), "missing {required}");
        }
    }

    #[test]
    fn cursor_catalog_rejects_generated_components() {
        assert!(CURSOR_COLUMN_SQL.contains("a.attgenerated::text AS generated_kind"));
        let generated = Row::new(
            vec!["generated_kind".into()],
            vec![DriverValue::Text("s".into())],
        );
        let error = reject_generated_cursor_column(&generated, &spec(), "tenant_id")
            .expect_err("stored generated cursors are unstable row locators");
        assert!(error.to_string().contains("is generated"));
        assert!(error.to_string().contains("UPDATE OF cursor trigger"));

        let ordinary = Row::new(
            vec!["generated_kind".into()],
            vec![DriverValue::Text(String::new())],
        );
        reject_generated_cursor_column(&ordinary, &spec(), "tenant_id")
            .expect("ordinary stored columns remain eligible");
    }

    #[test]
    fn guard_identity_and_body_cover_the_whole_tuple() {
        let version = MigrationId::derive("pg_guard_test", b"v1");
        let first = guard_identity(&version, &checksum(), &spec());
        let second = guard_identity(&version, &checksum(), &spec());
        assert_eq!(first, second);
        assert!(first.trigger_name.len() <= 63);
        assert!(first.function_name.len() <= 63);
        let body = guard_function_body(&spec()).unwrap();
        assert!(body.contains(
            "OLD.\"tenant_id\"::text COLLATE \"pg_catalog\".\"C\" IS DISTINCT FROM NEW.\"tenant_id\"::text COLLATE \"pg_catalog\".\"C\""
        ));
        assert!(body.contains(
            "OLD.\"id\"::text COLLATE \"pg_catalog\".\"C\" IS DISTINCT FROM NEW.\"id\"::text COLLATE \"pg_catalog\".\"C\""
        ));
        assert!(VERIFY_GUARD_SQL.contains("t.tgattr"));
        assert!(VERIFY_GUARD_SQL.contains("t.tgqual IS NULL"));
        assert!(VERIFY_GUARD_SQL.contains("t.tgnargs"));
        assert!(VERIFY_GUARD_SQL.contains("p.prosrc"));
        assert!(VERIFY_GUARD_SQL.contains("obj_description"));
    }

    fn online_rename_row(
        allowed: &AllowedOnlineRenameTrigger,
        enabled: &str,
        no_when_clause: bool,
        function_body: String,
    ) -> Row {
        Row::new(
            vec![
                "trigger_schema".into(),
                "trigger_table".into(),
                "trigger_name".into(),
                "trigger_type".into(),
                "trigger_enabled".into(),
                "no_when_clause".into(),
                "trigger_arg_count".into(),
                "function_schema".into(),
                "function_name".into(),
                "function_arg_count".into(),
                "function_language".into(),
                "security_definer".into(),
                "function_config".into(),
                "function_body".into(),
            ],
            vec![
                DriverValue::Text("app".into()),
                DriverValue::Text("users".into()),
                DriverValue::Text(allowed.trigger_name.clone()),
                DriverValue::Int(23),
                DriverValue::Text(enabled.into()),
                DriverValue::Bool(no_when_clause),
                DriverValue::Int(0),
                DriverValue::Text("app".into()),
                DriverValue::Text(allowed.function_name.clone()),
                DriverValue::Int(0),
                DriverValue::Text("plpgsql".into()),
                DriverValue::Bool(false),
                DriverValue::TextArray(Vec::new()),
                DriverValue::Text(function_body),
            ],
        )
    }

    #[test]
    fn online_rename_exception_requires_the_exact_owned_trigger_shape() {
        let allowed = AllowedOnlineRenameTrigger::new(
            "zsdw_users_email_email_address_trg".into(),
            "zsdw_users_email_email_address_fn".into(),
            "email".into(),
            "email_address".into(),
        );
        let body = crate::render::expand_contract::dual_write_function_body(
            "\"email\"",
            "\"email_address\"",
        );
        prove_allowed_engine_trigger(
            &online_rename_row(&allowed, "O", true, body.clone()),
            &spec(),
            &allowed,
        )
        .expect("the canonical online-rename trigger is allowed");

        for (enabled, no_when_clause, function_body) in [
            ("R", true, body.clone()),
            ("O", false, body.clone()),
            ("O", true, "BEGIN RETURN NEW; END".into()),
        ] {
            prove_allowed_engine_trigger(
                &online_rename_row(&allowed, enabled, no_when_clause, function_body),
                &spec(),
                &allowed,
            )
            .expect_err("a replica-only, qualified, or body-tampered trigger is unproven");
        }

        let mut cursor_is_renamed_column = spec();
        cursor_is_renamed_column.cursor_columns = vec!["email".into()];
        prove_allowed_engine_trigger(
            &online_rename_row(&allowed, "O", true, body),
            &cursor_is_renamed_column,
            &allowed,
        )
        .expect_err("dual-write may not touch a cursor component");
    }

    fn progress_relation_row(attribute_count: i64) -> Row {
        Row::new(
            vec![
                "relation_kind".into(),
                "persistence".into(),
                "attribute_count".into(),
            ],
            vec![
                DriverValue::Text("r".into()),
                DriverValue::Text("p".into()),
                DriverValue::Int(attribute_count),
            ],
        )
    }

    fn progress_column_row(
        index: usize,
        spec: ProgressColumnSpec,
        data_type: &str,
        not_null: bool,
    ) -> Row {
        Row::new(
            vec![
                "ordinal_position".into(),
                "column_name".into(),
                "data_type".into(),
                "not_null".into(),
                "dropped".into(),
                "identity_kind".into(),
                "generated_kind".into(),
                "default_expr".into(),
            ],
            vec![
                DriverValue::Int(i64::try_from(index + 1).unwrap()),
                DriverValue::Text(spec.name.into()),
                DriverValue::Text(data_type.into()),
                DriverValue::Bool(not_null),
                DriverValue::Bool(false),
                DriverValue::Text(String::new()),
                DriverValue::Text(String::new()),
                spec.default_expr
                    .map_or(DriverValue::Null, |value| DriverValue::Text(value.into())),
            ],
        )
    }

    fn exact_progress_column_rows() -> Vec<Row> {
        PROGRESS_COLUMNS
            .iter()
            .copied()
            .enumerate()
            .map(|(index, spec)| progress_column_row(index, spec, spec.data_type, spec.not_null))
            .collect()
    }

    fn progress_primary_key(columns: &[&str]) -> Row {
        Row::new(
            vec!["constraint_type".into(), "constraint_columns".into()],
            vec![
                DriverValue::Text("p".into()),
                DriverValue::TextArray(
                    columns
                        .iter()
                        .map(|column| Some((*column).to_string()))
                        .collect(),
                ),
            ],
        )
    }

    #[test]
    fn progress_schema_verifier_rejects_every_stale_layout_dimension() {
        let relation = progress_relation_row(PROGRESS_COLUMNS.len() as i64);
        let exact_columns = exact_progress_column_rows();
        let exact_constraints = [progress_primary_key(&["backfill_id"])];
        verify_progress_schema(&relation, &exact_columns, &exact_constraints)
            .expect("canonical progress schema");

        let mut extra_columns = exact_progress_column_rows();
        extra_columns.push(progress_column_row(
            PROGRESS_COLUMNS.len(),
            ProgressColumnSpec {
                name: "stale_extra",
                data_type: "text",
                not_null: false,
                default_expr: None,
            },
            "text",
            false,
        ));
        assert!(verify_progress_schema(
            &progress_relation_row(extra_columns.len() as i64),
            &extra_columns,
            &exact_constraints,
        )
        .is_err());

        let mut reordered = exact_progress_column_rows();
        reordered.swap(0, 1);
        assert!(verify_progress_schema(&relation, &reordered, &exact_constraints).is_err());

        let mut wrong_type = exact_progress_column_rows();
        wrong_type[5] = progress_column_row(5, PROGRESS_COLUMNS[5], "json", true);
        assert!(verify_progress_schema(&relation, &wrong_type, &exact_constraints).is_err());

        let mut wrong_nullability = exact_progress_column_rows();
        wrong_nullability[8] = progress_column_row(8, PROGRESS_COLUMNS[8], "jsonb", true);
        assert!(verify_progress_schema(&relation, &wrong_nullability, &exact_constraints).is_err());

        assert!(verify_progress_schema(
            &relation,
            &exact_columns,
            &[progress_primary_key(&["checksum"])],
        )
        .is_err());
    }

    #[test]
    fn cohort_integrity_changes_with_the_terminal_tuple() {
        let contract = contract();
        let left = CursorTuple::new(
            vec![IrScalar::Int64(5), IrScalar::Str("a".into())],
            &contract,
        )
        .unwrap();
        let right = CursorTuple::new(
            vec![IrScalar::Int64(5), IrScalar::Str("b".into())],
            &contract,
        )
        .unwrap();
        assert_ne!(
            cohort_bound_checksum(&checksum(), &contract, Some(&left)).unwrap(),
            cohort_bound_checksum(&checksum(), &contract, Some(&right)).unwrap()
        );
        assert_ne!(
            cohort_bound_checksum(&checksum(), &contract, Some(&left)).unwrap(),
            cohort_bound_checksum(&checksum(), &contract, None).unwrap()
        );
    }

    #[test]
    fn external_invariant_is_part_of_the_persisted_contract() {
        let mut value = spec();
        value.cursor_stability = CursorStability::ExternalInvariant {
            name: "users_identity_is_immutable".into(),
        };
        let encoded = json_encode(&value.cursor_stability, "cursor stability").unwrap();
        assert_eq!(
            encoded,
            r#"{"mode":"externalInvariant","name":"users_identity_is_immutable"}"#
        );
        assert!(validate_spec(&value).is_ok());
    }
}
