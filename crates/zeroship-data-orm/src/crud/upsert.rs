//! Resolve a prepared upsert into physical columns before SQL compilation.

use crate::{
    sql::{
        CompareOp, Ident, IdentRole, SchemaName,
        compile::{self, QueryError, SqlDialect},
        compiler::{CompiledQuery, PostgresCompiler, SqlCompiler, SqliteCompiler},
        lifecycle::{AssignedValue, WriteAssignments},
        statement::{
            Assignment, Comparison, Expression, ReturnedColumn, Statement, StorageType, Table,
            Upsert, UpsertParts,
        },
    },
    value::Value,
};
use std::collections::{BTreeMap, HashSet};

pub fn build_upsert(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    document: Value,
    conflict: &Value,
) -> Result<CompiledQuery, QueryError> {
    build_upsert_with_dialect(
        namespace,
        collection,
        schema,
        document,
        conflict,
        SqlDialect::Postgres,
    )
}

pub fn build_upsert_with_dialect(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    document: Value,
    conflict: &Value,
    dialect: SqlDialect,
) -> Result<CompiledQuery, QueryError> {
    build_upsert_with_assignments(
        namespace,
        collection,
        schema,
        document,
        conflict,
        dialect,
        &WriteAssignments::default(),
        None,
    )
}

pub fn build_upsert_with_assignments(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    document: Value,
    conflict: &Value,
    dialect: SqlDialect,
    assignments: &WriteAssignments,
    expected_key: Option<crate::value::Record>,
) -> Result<CompiledQuery, QueryError> {
    let statement = resolve(
        namespace,
        collection,
        schema,
        document,
        conflict,
        dialect,
        assignments,
        expected_key,
    )?;
    let compiler: &dyn SqlCompiler = match dialect {
        SqlDialect::Postgres => &PostgresCompiler,
        SqlDialect::Sqlite => &SqliteCompiler,
    };
    compiler
        .compile(statement, &compiler.support())
        .map_err(Into::into)
}

pub fn resolve(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    document: Value,
    conflict: &Value,
    dialect: SqlDialect,
    assignments: &WriteAssignments,
    expected_key: Option<crate::value::Record>,
) -> Result<Statement, QueryError> {
    compile::validate_collection(collection)?;
    let fields = schema
        .as_object()
        .ok_or_else(|| invalid("upsert requires a field-map schema"))?;
    let Value::Object(mut document) = document else {
        return Err(invalid("upsert document must be an object"));
    };
    let conflict = compile::parse_conflict_fields(conflict)?;
    for field in &conflict {
        compile::validate_value_operation(field, schema)?;
    }

    let mut physical = Vec::new();
    let mut inputs = BTreeMap::new();
    for (name, definition) in fields {
        if compile::is_schema_metadata_key(name) {
            continue;
        }
        let stored = compile::value_column_for_field(name, schema);
        let masked = crate::sql::descriptors::effective_mask(definition).is_some();
        let storage = if masked {
            StorageType::Text
        } else {
            storage_type(definition, dialect)?
        };
        physical.push((stored_ident(&stored)?, storage));
        let insert_only = definition["primaryKey"].as_bool() == Some(true)
            || definition["assign"]["on"].as_str() == Some("insert");
        inputs.insert(name.clone(), (stored.clone(), storage, insert_only));
        if let Some(raw) = compile::declared_raw_column(name, definition)? {
            let raw_storage = storage_type(definition, dialect)?;
            physical.push((stored_ident(&raw)?, raw_storage));
            inputs.insert(raw.clone(), (raw, raw_storage, insert_only));
        }
    }
    let table = Table::new(
        namespace.clone(),
        Ident::parse_as(collection, IdentRole::Collection)
            .map_err(crate::sql::compiler::CompileError::from)?,
        physical,
    )?;
    let conflict: Vec<_> = conflict
        .iter()
        .map(|name| {
            table
                .column(&compile::value_column_for_field(name, schema))
                .map_err(Into::into)
        })
        .collect::<Result<_, QueryError>>()?;
    let targets: HashSet<_> = conflict.iter().map(|c| c.name().as_str()).collect();
    let assigned: HashSet<_> = assignments
        .columns
        .iter()
        .map(|c| c.column.as_str())
        .collect();
    let insert_generated_identity = crate::sql::identity::generated_fields(schema)
        .iter()
        .any(|key| document.contains_key(*key));
    let mut insert = Vec::new();
    let mut update = Vec::new();
    document.sort_keys();
    for (name, value) in document {
        let (physical, storage, insert_only) = inputs
            .get(&name)
            .ok_or_else(|| invalid("input column is not declared by the descriptor"))?;
        let column = table.column(physical)?;
        if !insert_only
            && !targets.contains(physical.as_str())
            && !assigned.contains(physical.as_str())
        {
            update.push(Assignment {
                column: column.clone(),
                value: Expression::Incoming(column.clone()),
            });
        }
        insert.push(Assignment {
            column,
            value: expression(*storage, value, dialect)?,
        });
    }
    for assignment in &assignments.columns {
        let column = table.column(&assignment.column)?;
        let value = match &assignment.value {
            AssignedValue::CurrentTimestamp => Expression::CurrentTimestamp,
            AssignedValue::Increment(step) => Expression::Increment {
                column: column.clone(),
                step: *step,
            },
            AssignedValue::Bound(value) => {
                Expression::Bind(encode(column.storage(), value.clone(), dialect)?)
            }
        };
        update.push(Assignment { column, value });
    }
    // Preserve returning rows and trigger behavior when no input field changes.
    if update.is_empty() {
        let column = conflict.first().expect("validated conflict target").clone();
        update.push(Assignment {
            column: column.clone(),
            value: Expression::Incoming(column),
        });
    }
    let conditions = expected_key
        .map(|values| {
            let key = crate::row_identity::key(schema, &values)
                .map_err(|_| invalid("upsert guard requires a complete identity"))?;
            key.into_iter()
                .map(|(name, value)| {
                    let column = table.column(&compile::value_column_for_field(&name, schema))?;
                    let value = encode(column.storage(), value, dialect)?;
                    Ok(Comparison { column, op: CompareOp::Eq, value })
                })
                .collect::<Result<Vec<_>, QueryError>>()
        })
        .transpose()?
        .unwrap_or_default();
    let returning = compile::implicit_read_fields(schema)?
        .into_iter()
        .map(|name| returned(&table, name, schema))
        .collect::<Result<_, _>>()?;
    Ok(Statement::Upsert(Upsert::new(UpsertParts {
        table,
        insert,
        conflict,
        update,
        conditions,
        returning,
        insert_generated_identity,
    })?))
}

fn returned(table: &Table, name: &str, schema: &Value) -> Result<ReturnedColumn, QueryError> {
    let physical = compile::value_column_for_field(name, schema);
    let alias = if physical == name {
        None
    } else {
        Some(
            Ident::parse_as(name, IdentRole::Alias)
                .map_err(crate::sql::compiler::CompileError::from)?,
        )
    };
    Ok(ReturnedColumn {
        column: table.column(&physical)?,
        alias,
    })
}

fn stored_ident(name: &str) -> Result<Ident, QueryError> {
    Ident::parse_as(name, IdentRole::StoredColumn)
        .map_err(|e| crate::sql::compiler::CompileError::from(e).into())
}

fn invalid(message: &str) -> QueryError {
    QueryError::InvalidFilter(message.into())
}

fn storage_type(definition: &Value, dialect: SqlDialect) -> Result<StorageType, QueryError> {
    if crate::sql::descriptors::is_encrypted(definition) {
        return Ok(StorageType::Bytes);
    }
    Ok(match definition["type"].as_str() {
        Some("string" | "text" | "id" | "calendarDate") => StorageType::Text,
        Some("boolean" | "bool") if dialect == SqlDialect::Sqlite => StorageType::Integer,
        Some("boolean" | "bool") => StorageType::Boolean,
        Some("integer" | "int" | "bigint" | "bigInt") => StorageType::Integer,
        Some("number" | "float" | "double") => StorageType::Real,
        Some("decimal") => StorageType::Decimal,
        Some("bytes") => StorageType::Bytes,
        Some("date" | "timestamp" | "timestamptz") => StorageType::Timestamp,
        Some("json" | "object" | "array" | "union") => StorageType::Json,
        Some("vector" | "geoPoint") if dialect == SqlDialect::Sqlite => StorageType::Bytes,
        Some("vector") => StorageType::Vector,
        Some("geoPoint") => StorageType::GeoPoint,
        _ => return Err(invalid("descriptor has no supported storage type")),
    })
}

fn expression(
    storage: StorageType,
    value: Value,
    dialect: SqlDialect,
) -> Result<Expression, QueryError> {
    if value.is_null() {
        return Ok(Expression::Null);
    }
    Ok(Expression::Bind(encode(storage, value, dialect)?))
}

fn encode(storage: StorageType, value: Value, dialect: SqlDialect) -> Result<Value, QueryError> {
    if value.is_null() {
        return Ok(value);
    }
    Ok(match (storage, value) {
        (StorageType::Timestamp, value) => {
            let millis = crate::sql::temporal::timestamp_millis(&value)
                .ok_or_else(|| invalid("invalid timestamp storage value"))?;
            if dialect == SqlDialect::Sqlite {
                Value::String(
                    crate::sql::temporal::format_timestamp_millis(millis)
                        .expect("validated timestamp"),
                )
            } else {
                Value::Timestamp(millis)
            }
        }
        (StorageType::Integer, Value::Bool(value)) if dialect == SqlDialect::Sqlite => {
            Value::from(i64::from(value))
        }
        (StorageType::Text, Value::Timestamp(value)) if dialect == SqlDialect::Sqlite => {
            Value::String(
                crate::sql::temporal::format_timestamp_millis(value)
                    .ok_or_else(|| invalid("invalid timestamp storage value"))?,
            )
        }
        (StorageType::Json, value)
            if !matches!(value, Value::Json(_) | Value::Array(_) | Value::Object(_)) =>
        {
            Value::Json(value.to_string())
        }
        (_, value) => value,
    })
}
