//! SQL for ORM-owned protection operations. Caller values remain parameters.
use crate::sql::compile::{
    QueryError, SqlDialect, push_field_value_bind, quote_ident_for_dialect, value_column_for_field,
};
use crate::sql::compiler::CompiledQuery;
use crate::value::{Record, Value};

pub const AUDIT_UNMASK_TABLE: &str = "__zeroship_audit_unmask";

fn placeholder(dialect: SqlDialect, index: usize) -> String {
    match dialect {
        SqlDialect::Postgres => format!("${index}"),
        SqlDialect::Sqlite => format!("?{index}"),
    }
}

pub fn raw_column(
    namespace: &str,
    collection: &str,
    column: &str,
    key: &Record,
    schema: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    let mut params = Vec::new();
    let mut predicates = Vec::new();
    for name in crate::sql::descriptors::primary_key_fields(schema)
        .map_err(|message| QueryError::InvalidFilter(message.into()))?
    {
        let value = key
            .get(name)
            .filter(|value| !value.is_null())
            .ok_or_else(|| QueryError::InvalidFilter("raw reads require a complete key".into()))?;
        let bind = push_field_value_bind(&mut params, value, name, schema, dialect)?;
        predicates.push(format!(
            "{} = {bind}",
            quote_ident_for_dialect(&value_column_for_field(name, schema), dialect)
        ));
    }
    let predicate = predicates.join(" AND ");
    Ok(CompiledQuery {
        sql: format!(
            "SELECT {} FROM {}.{} WHERE {predicate}",
            quote_ident_for_dialect(column, dialect),
            quote_ident_for_dialect(namespace, dialect),
            quote_ident_for_dialect(collection, dialect),
        ),
        params,
    })
}

pub fn unmask_audit(namespace: &str, dialect: SqlDialect, params: Vec<Value>) -> CompiledQuery {
    let columns = [
        "actor_id",
        "actor_role",
        "claimed_actor",
        "collection",
        "row_pk",
        "column",
        "classification",
        "reason",
        "outcome",
    ];
    let placeholders = (1..=columns.len())
        .map(|index| placeholder(dialect, index))
        .collect::<Vec<_>>()
        .join(", ");
    CompiledQuery {
        sql: format!(
            "INSERT INTO {}.{} ({}) VALUES ({placeholders})",
            quote_ident_for_dialect(namespace, dialect),
            quote_ident_for_dialect(AUDIT_UNMASK_TABLE, dialect),
            columns
                .map(|column| quote_ident_for_dialect(column, dialect))
                .join(", ")
        ),
        params,
    }
}
