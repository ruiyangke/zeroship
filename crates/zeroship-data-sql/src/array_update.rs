//! Atomic JSON array mutations. Each operand is one complete array element.
use crate::compile::{QueryError, SqlDialect};

#[derive(Clone, Copy)]
pub enum ArrayUpdate {
    Push,
    Pull,
    AddToSet,
}

/// `column` is an already-quoted identifier; `bind` is a generated placeholder.
pub fn render(
    dialect: SqlDialect,
    operation: ArrayUpdate,
    column: &str,
    bind: &str,
) -> Result<String, QueryError> {
    let expression = match dialect {
        SqlDialect::Postgres => {
            let append = format!(
                "jsonb_insert({column}, ARRAY[jsonb_array_length({column})::text], {bind}::jsonb)"
            );
            let mutation = match operation {
                ArrayUpdate::Push => append,
                ArrayUpdate::Pull => format!(
                    "(SELECT COALESCE(jsonb_agg(__zs_array.element ORDER BY __zs_array.position), '[]'::jsonb) \
                     FROM jsonb_array_elements({column}) WITH ORDINALITY AS __zs_array(element, position) \
                     WHERE __zs_array.element != {bind}::jsonb)"
                ),
                ArrayUpdate::AddToSet => format!(
                    "CASE WHEN EXISTS \
                     (SELECT 1 FROM jsonb_array_elements({column}) AS __zs_array(element) \
                     WHERE __zs_array.element = {bind}::jsonb) \
                     THEN {column} ELSE {append} END"
                ),
            };
            format!(
                "CASE WHEN {column} IS NULL OR jsonb_typeof({column}) = 'null' THEN {column} ELSE {mutation} END"
            )
        }
        SqlDialect::Sqlite => {
            // Capture the outer column before json_each introduces its own
            // `value`, `key`, and `type` columns. Extract JSON text to preserve
            // booleans, nulls, nested containers, and exact numeric lexemes.
            let elements = format!(
                "WITH __zs_input(document) AS (SELECT {column}), \
                 __zs_elements(position, element) AS \
                 (SELECT __zs_each.key, __zs_input.document -> __zs_each.key \
                 FROM __zs_input, json_each(__zs_input.document) AS __zs_each)"
            );
            let append = format!("json_insert({column}, '$[#]', json({bind}))");
            let mutation = match operation {
                ArrayUpdate::Push => append,
                ArrayUpdate::Pull => format!(
                    "({elements} SELECT json_group_array(json(element) ORDER BY position) \
                     FROM __zs_elements WHERE NOT zeroship_json_equal(element, {bind}))"
                ),
                ArrayUpdate::AddToSet => format!(
                    "CASE WHEN EXISTS ({elements} SELECT 1 FROM __zs_elements \
                     WHERE zeroship_json_equal(element, {bind})) THEN {column} ELSE {append} END"
                ),
            };
            // SQLite's JSON insertion silently ignores a non-array target.
            // Refuse that shape instead of reporting a successful no-op.
            format!(
                "CASE WHEN {column} IS NULL OR json_type({column}) = 'null' THEN {column} WHEN json_type({column}) = 'array' \
                 THEN {mutation} ELSE json('') END"
            )
        }
        SqlDialect::Mysql => {
            return Err(QueryError::InvalidFilter(
                "array updates are unsupported for MySQL".into(),
            ));
        }
    };
    Ok(format!("{column} = {expression}"))
}
