//! Dialect plans for reserving database-generated identities before encryption.
use crate::sql::compiler::CompiledQuery;

use crate::sql::{
    SchemaName,
    compile::{
        MAX_INSERT_MANY_BATCH, QueryError, SqlDialect, quote_ident, validate_collection,
        value_column_for_field,
    },
};
use crate::value::Value;

pub fn generated_fields(schema: &Value) -> Vec<&str> {
    schema
        .as_object()
        .into_iter()
        .flat_map(|fields| fields.iter())
        .filter(|(_, field)| {
            field["primaryKey"].as_bool() == Some(true)
                && field["assign"]["by"].as_str() == Some("identity")
                && field["assign"]["on"].as_str() == Some("insert")
        })
        .map(|(name, _)| name.as_str())
        .collect()
}

pub fn is_generated(schema: &Value) -> bool {
    !generated_fields(schema).is_empty()
}

#[derive(Debug)]
pub enum Allocation {
    Sequence(CompiledQuery),
    RowId {
        maximum: CompiledQuery,
        has_sequence: CompiledQuery,
        sequence: CompiledQuery,
    },
}

/// The caller must retain SQLite's write reservation until insertion settles.
pub fn reserve_sqlite_writer(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
) -> Result<CompiledQuery, QueryError> {
    validate_collection(collection)?;
    let table = format!(
        "{}.{}",
        crate::sql::compile::quote_ident(namespace.as_str()),
        quote_ident(collection)
    );
    let key = crate::sql::descriptors::primary_key_fields(schema)
        .map_err(|message| QueryError::InvalidFilter(message.into()))?;
    let id = quote_ident(&value_column_for_field(key[0], schema));
    Ok(CompiledQuery {
        sql: format!("UPDATE {table} SET {id} = {id} WHERE FALSE"),
        params: vec![],
    })
}

pub fn allocation(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    dialect: SqlDialect,
    count: usize,
    field: &str,
) -> Result<Allocation, QueryError> {
    validate_collection(collection)?;
    if count == 0 || count > MAX_INSERT_MANY_BATCH {
        return Err(QueryError::InvalidFilter(
            "identity allocation exceeds the insert batch limit".into(),
        ));
    }
    let table = format!(
        "{}.{}",
        crate::sql::compile::quote_ident(namespace.as_str()),
        quote_ident(collection)
    );
    let id = value_column_for_field(field, schema);
    match dialect {
        SqlDialect::Postgres => Ok(Allocation::Sequence(CompiledQuery {
            sql:"SELECT nextval(pg_get_serial_sequence($1, $2)) AS id FROM generate_series(1, $3::integer)".into(),
            params:vec![Value::from(table), Value::from(id), Value::from(count as i64)],
        })),
        SqlDialect::Sqlite => Ok(Allocation::RowId {
            maximum: CompiledQuery { sql:format!("SELECT COALESCE(MAX({}), 0) AS id FROM {table}", quote_ident(&id)), params:vec![] },
            has_sequence: CompiledQuery { sql:format!("SELECT name FROM {}.sqlite_schema WHERE type = 'table' AND name = 'sqlite_sequence'", crate::sql::compile::quote_ident(namespace.as_str())), params:vec![] },
            sequence: CompiledQuery { sql:format!("SELECT seq AS id FROM {}.sqlite_sequence WHERE name = $1", crate::sql::compile::quote_ident(namespace.as_str())), params:vec![Value::from(collection)] },
        }),
    }
}

pub(crate) fn overriding_clause(
    schema: &Value,
    dialect: SqlDialect,
    has_generated_key: bool,
) -> &'static str {
    if dialect == SqlDialect::Postgres && is_generated(schema) && has_generated_key {
        " OVERRIDING SYSTEM VALUE"
    } else {
        ""
    }
}
