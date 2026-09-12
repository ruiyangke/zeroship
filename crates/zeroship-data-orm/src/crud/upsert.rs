//! Resolve a prepared upsert into physical columns before SQL compilation.

use super::resolved::ResolvedTable;
use crate::{
    sql::{
        compiler::{CompiledQuery, Requirements},
        lifecycle::{AssignedValue, WriteAssignments},
        mapping::{self, QueryError},
        statement::{
            Assignment, Comparison, Expression, Statement, StorageType, Upsert, UpsertParts,
        },
        CompareOp, SchemaName,
    },
    value::Value,
};
use std::collections::HashSet;

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
    let allocates_identity = guard_identity && super::identity::is_generated(schema);
    Requirements {
        relational_reads: false,
        aggregate_reads: false,
        vector_search: false,
        inner_product_vector_search: false,
        spatial_search: false,
        explicit_conflict_target: true,
        conditional_conflict_update: guard_identity,
        returning: true,
        insert_generated_identity: allocates_identity,
        identity_allocation: allocates_identity,
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
    mapping::validate_collection(collection)?;
    let Value::Object(mut document) = document else {
        return Err(invalid("upsert document must be an object"));
    };
    let conflict = mapping::parse_conflict_fields(conflict)?;
    for field in &conflict {
        mapping::validate_value_operation(field, schema)?;
    }

    let resolved = ResolvedTable::new(namespace, collection, schema, registration)?;
    let table = &resolved.table;
    let conflict: Vec<_> = conflict
        .iter()
        .map(|name| {
            table
                .column(&mapping::value_column_for_field(name, schema))
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
        super::identity::is_generated(schema) && document.contains_key("id");
    let mut insert = Vec::new();
    let mut update = Vec::new();
    document.sort_keys();
    for (name, value) in document {
        let input = resolved
            .inputs
            .get(&name)
            .ok_or_else(|| invalid("input column is not declared by the descriptor"))?;
        let column = table.column(&input.column)?;
        if !input.insert_only
            && !targets.contains(input.column.as_str())
            && !assigned.contains(input.column.as_str())
        {
            update.push(Assignment {
                column: column.clone(),
                value: Expression::Incoming(column.clone()),
            });
        }
        insert.push(Assignment {
            column,
            value: expression(input.storage, value, registration)?,
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
            let column = table.column(&mapping::value_column_for_field("id", schema))?;
            let value = registration.encode(column.storage(), value)?;
            Ok::<_, QueryError>(Comparison {
                column,
                op: CompareOp::Eq,
                value,
            })
        })
        .transpose()?;
    let returning = resolved.returning(schema)?;
    Ok(Statement::Upsert(Upsert::new(UpsertParts {
        table: resolved.table,
        insert,
        conflict,
        update,
        condition,
        returning,
        insert_generated_identity,
    })?))
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
