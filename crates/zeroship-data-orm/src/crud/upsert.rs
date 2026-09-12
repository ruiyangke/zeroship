//! Resolve a prepared upsert into physical columns before SQL compilation.

use crate::{
    sql::{
        CompareOp, Ident, IdentRole, SchemaName,
        compile::{self, QueryError, SqlDialect},
        compiler::{CompiledQuery, Requirements},
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
    expected_id: Option<Value>,
) -> Result<CompiledQuery, QueryError> {
    let registration = crate::sql::registration::SqlRegistration::builtin(dialect);
    build_upsert_with_registration(
        namespace,
        collection,
        schema,
        document,
        conflict,
        assignments,
        expected_id,
        &registration,
    )
}

pub(crate) fn build_upsert_with_registration(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    document: Value,
    conflict: &Value,
    assignments: &WriteAssignments,
    expected_id: Option<Value>,
    registration: &crate::sql::registration::SqlRegistration,
) -> Result<CompiledQuery, QueryError> {
    let statement = resolve(
        namespace,
        collection,
        schema,
        document,
        conflict,
        assignments,
        expected_id,
        registration,
    )?;
    registration.compile(statement).map_err(Into::into)
}

pub(crate) fn requirements(schema: &Value, guard_identity: bool) -> Requirements {
    Requirements {
        explicit_conflict_target: true,
        conditional_conflict_update: guard_identity,
        returning: true,
        insert_generated_identity: crate::sql::identity::is_generated(schema),
        default_expression: false,
        bind_parameters: 0,
    }
}

fn resolve(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    document: Value,
    conflict: &Value,
    assignments: &WriteAssignments,
    expected_id: Option<Value>,
    registration: &crate::sql::registration::SqlRegistration,
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
            registration.storage_type(definition)?
        };
        physical.push((stored_ident(&stored)?, storage));
        let insert_only = name == "id" || definition["assign"]["on"].as_str() == Some("insert");
        inputs.insert(name.clone(), (stored.clone(), storage, insert_only));
        if let Some(raw) = compile::declared_raw_column(name, definition)? {
            let raw_storage = registration.storage_type(definition)?;
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
    let insert_generated_identity =
        crate::sql::identity::is_generated(schema) && document.contains_key("id");
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
            value: expression(*storage, value, registration)?,
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
                Expression::Bind(registration.encode(column.storage(), value.clone())?)
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
    let condition = expected_id
        .map(|value| {
            let column = table.column(&compile::value_column_for_field("id", schema))?;
            let value = registration.encode(column.storage(), value)?;
            Ok::<_, QueryError>(Comparison {
                column,
                op: CompareOp::Eq,
                value,
            })
        })
        .transpose()?;
    let returning = compile::implicit_read_fields(schema)?
        .into_iter()
        .map(|name| returned(&table, name, schema))
        .collect::<Result<_, _>>()?;
    Ok(Statement::Upsert(Upsert::new(UpsertParts {
        table,
        insert,
        conflict,
        update,
        condition,
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

fn expression(
    storage: StorageType,
    value: Value,
    registration: &crate::sql::registration::SqlRegistration,
) -> Result<Expression, QueryError> {
    if value.is_null() {
        return Ok(Expression::Null);
    }
    Ok(Expression::Bind(registration.encode(storage, value)?))
}
