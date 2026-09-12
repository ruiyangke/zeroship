//! Resolve inserts into physical columns before backend compilation.

use super::resolved::ResolvedTable;
use crate::{
    sql::{
        compile::{self, QueryError},
        compiler::{CompiledQuery, Requirements},
        registration::SqlRegistration,
        statement::{Expression, Insert, InsertParts, Statement},
        SchemaName,
    },
    value::{Record, Value},
};
use std::collections::BTreeSet;

pub(crate) fn requirements(schema: &Value) -> Requirements {
    Requirements {
        returning: true,
        insert_generated_identity: crate::sql::identity::is_generated(schema),
        ..Requirements::default()
    }
}

pub(crate) fn build_one(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    document: Value,
    registration: &SqlRegistration,
) -> Result<CompiledQuery, QueryError> {
    let Value::Object(document) = document else {
        return Err(invalid("insert document must be an object"));
    };
    compile(namespace, collection, schema, vec![document], registration)
}

pub(crate) fn build_many(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    documents: Value,
    registration: &SqlRegistration,
) -> Result<CompiledQuery, QueryError> {
    let Value::Array(documents) = documents else {
        return Err(invalid("insertMany documents must be an array"));
    };
    if documents.len() > compile::MAX_INSERT_MANY_BATCH {
        return Err(invalid("insertMany exceeds the batch limit"));
    }
    let documents = documents
        .into_iter()
        .map(|document| match document {
            Value::Object(document) => Ok(document),
            _ => Err(invalid("insertMany documents must be objects")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    compile(namespace, collection, schema, documents, registration)
}

fn compile(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    mut documents: Vec<Record>,
    registration: &SqlRegistration,
) -> Result<CompiledQuery, QueryError> {
    if documents.is_empty() {
        return Err(invalid("insert requires at least one document"));
    }
    let resolved = ResolvedTable::new(namespace, collection, schema, registration)?;
    let names: BTreeSet<_> = documents
        .iter()
        .flat_map(|document| document.keys().cloned())
        .collect();
    if names.is_empty() {
        return Err(invalid("insert documents cannot be empty"));
    }
    let columns = names
        .iter()
        .map(|name| {
            let input = resolved
                .inputs
                .get(name)
                .ok_or_else(|| invalid("input column is not declared by the descriptor"))?;
            resolved.table.column(&input.column).map_err(Into::into)
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    let insert_generated_identity = crate::sql::identity::is_generated(schema)
        && documents.iter().any(|document| document.contains_key("id"));
    let mut rows = Vec::with_capacity(documents.len());
    for document in &mut documents {
        let mut row = Vec::with_capacity(columns.len());
        for name in &names {
            let input = resolved.inputs.get(name).expect("resolved insert column");
            let value = document.shift_remove(name).unwrap_or(Value::Null);
            row.push(if value.is_null() {
                Expression::Null
            } else {
                Expression::Bind(registration.encode(input.storage, value)?)
            });
        }
        rows.push(row);
    }
    let returning = resolved.returning(schema)?;
    registration
        .compile(Statement::Insert(Insert::new(InsertParts {
            table: resolved.table,
            columns,
            rows,
            returning,
            insert_generated_identity,
        })?))
        .map_err(Into::into)
}

fn invalid(message: &str) -> QueryError {
    QueryError::InvalidFilter(message.into())
}
