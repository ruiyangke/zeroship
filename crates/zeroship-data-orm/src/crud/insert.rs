//! Resolve inserts into physical columns before backend compilation.

use super::resolved::ResolvedTable;
use crate::{
    sql::{
        compile::QueryError,
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
        insert_generated_identity: super::identity::is_generated(schema),
        identity_allocation: super::identity::is_generated(schema),
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
) -> Result<Vec<CompiledQuery>, QueryError> {
    let Value::Array(documents) = documents else {
        return Err(invalid("insertMany documents must be an array"));
    };
    if documents.len() > crate::budgets::MAX_INSERT_MANY_BATCH {
        return Err(invalid("insertMany exceeds the batch limit"));
    }
    let documents = documents
        .into_iter()
        .map(|document| match document {
            Value::Object(document) => Ok(document),
            _ => Err(invalid("insertMany documents must be objects")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if registration.support().default_expression || documents_share_columns(&documents) {
        return compile(namespace, collection, schema, documents, registration)
            .map(|query| vec![query]);
    }

    let mut groups: Vec<Vec<Record>> = Vec::new();
    for document in documents {
        let columns: BTreeSet<_> = document.keys().cloned().collect();
        if groups.last().is_some_and(|group| {
            group
                .first()
                .is_some_and(|first| first.keys().cloned().collect::<BTreeSet<_>>() == columns)
        }) {
            groups
                .last_mut()
                .expect("existing insert group")
                .push(document);
        } else {
            groups.push(vec![document]);
        }
    }
    groups
        .into_iter()
        .map(|group| compile(namespace, collection, schema, group, registration))
        .collect()
}

fn documents_share_columns(documents: &[Record]) -> bool {
    let Some(first) = documents.first() else {
        return true;
    };
    let first: BTreeSet<_> = first.keys().collect();
    documents
        .iter()
        .skip(1)
        .all(|document| document.keys().collect::<BTreeSet<_>>() == first)
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
    let insert_generated_identity = super::identity::is_generated(schema)
        && documents.iter().any(|document| document.contains_key("id"));
    let mut rows = Vec::with_capacity(documents.len());
    for document in &mut documents {
        let mut row = Vec::with_capacity(columns.len());
        for name in &names {
            let input = resolved.inputs.get(name).expect("resolved insert column");
            row.push(match document.shift_remove(name) {
                None => Expression::Default,
                Some(Value::Null) => Expression::Null,
                Some(value) => Expression::Bind(registration.encode(input.storage, value)?),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{
        compiler::{CompileError, SqlCompiler, SqliteCompiler},
        registration::SqlStorageCodecs,
        statement::StorageType,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[derive(Clone)]
    struct CountingCodecs(Arc<AtomicUsize>);

    impl SqlStorageCodecs for CountingCodecs {
        fn storage_type(&self, definition: &Value) -> Result<StorageType, CompileError> {
            SqlRegistration::sqlite().storage_type(definition)
        }

        fn encode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            SqlRegistration::sqlite().encode(storage, value)
        }

        fn decode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
            SqlRegistration::sqlite().decode(storage, value)
        }
    }

    #[test]
    fn insert_values_cross_the_registered_codec_once_and_json_stays_native() {
        let calls = Arc::new(AtomicUsize::new(0));
        let compiler = SqliteCompiler;
        let registration = SqlRegistration::new(
            "counting-codecs",
            crate::sql::registration::SQLITE_FAMILY,
            compiler,
            CountingCodecs(calls.clone()),
            compiler.support(),
        )
        .unwrap();
        let queries = build_many(
            &SchemaName::new("app").unwrap(),
            "entries",
            &crate::value!({
                "id":{"type":"string","primaryKey":true},
                "payload":{"type":"json"}
            }),
            crate::value!([{"id":"entry_a","payload":{"nested":[true]}}]),
            &registration,
        )
        .unwrap();

        let params: Vec<_> = queries.iter().flat_map(|query| query.params()).collect();
        assert_eq!(calls.load(Ordering::Relaxed), params.len());
        assert!(params.iter().any(|value| matches!(value, Value::Object(_))));
    }

    #[test]
    fn insert_many_distinguishes_absent_fields_from_null() {
        let queries = build_many(
            &SchemaName::new("app").unwrap(),
            "entries",
            &crate::value!({
                "id":{"type":"string","primaryKey":true},
                "nickname":{"type":"string"}
            }),
            crate::value!([
                {"id":"entry_a","nickname":null},
                {"id":"entry_b"}
            ]),
            &SqlRegistration::postgres(),
        )
        .unwrap();
        let query = &queries[0];

        assert!(query.sql().contains("VALUES ($1, NULL), ($2, DEFAULT)"));
        assert_eq!(
            query.params(),
            &[crate::value!("entry_a"), crate::value!("entry_b")]
        );
    }

    #[test]
    fn sqlite_groups_heterogeneous_rows_without_default_expressions() {
        let queries = build_many(
            &SchemaName::new("app").unwrap(),
            "entries",
            &crate::value!({
                "id":{"type":"string","primaryKey":true},
                "nickname":{"type":"string"}
            }),
            crate::value!([
                {"id":"entry_a","nickname":null},
                {"id":"entry_b"}
            ]),
            &SqlRegistration::sqlite(),
        )
        .unwrap();

        assert_eq!(queries.len(), 2);
        assert!(queries.iter().all(|query| !query.sql().contains("DEFAULT")));
        assert!(queries[0]
            .sql()
            .contains("(\"id\", \"nickname\") VALUES ($1, NULL)"));
        assert!(queries[1].sql().contains("(\"id\") VALUES ($1)"));
    }
}
