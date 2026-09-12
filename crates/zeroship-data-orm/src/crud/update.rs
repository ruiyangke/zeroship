use super::{predicate, resolved::ResolvedTable};
use crate::{
    sql::{
        compile::QueryError,
        lifecycle::{AssignedValue, WriteAssignments},
        registration::SqlRegistration,
        statement::{
            ArithmeticOperator, ArrayOperator, Assignment, Expression, MutationScope, Statement,
            Update, UpdateParts,
        },
        update::Operator,
        SchemaName,
    },
    value::Value,
};

pub(crate) fn build_one(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    filter: predicate::Input,
    update: Value,
    generated: &WriteAssignments,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    build(
        namespace,
        collection,
        schema,
        filter,
        update,
        generated,
        true,
        registration,
    )
}

pub(crate) fn build_many(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    filter: predicate::Input,
    update: Value,
    generated: &WriteAssignments,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    build(
        namespace,
        collection,
        schema,
        filter,
        update,
        generated,
        false,
        registration,
    )
}

fn build(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    filter: predicate::Input,
    update: Value,
    generated: &WriteAssignments,
    first: bool,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    let resolved = ResolvedTable::new(namespace, collection, schema, registration)?;
    let predicate = filter.resolve(schema, &resolved, registration)?;
    let assignments = resolve_assignments(update, generated, &resolved, registration)?;
    let scope = if first {
        MutationScope::First {
            target: resolved
                .table
                .column(&crate::sql::compile::value_column_for_field("id", schema))?,
        }
    } else {
        MutationScope::Matching
    };
    let returning = if first {
        resolved.returning(schema)?
    } else {
        Vec::new()
    };
    registration
        .compile(Statement::Update(Update::new(UpdateParts {
            table: resolved.table,
            assignments,
            predicate,
            scope,
            returning,
        })?))
        .map_err(Into::into)
}

pub(crate) fn resolve_assignments(
    update: Value,
    generated: &WriteAssignments,
    resolved: &ResolvedTable,
    registration: &SqlRegistration,
) -> Result<Vec<Assignment>, QueryError> {
    let mut assignments = crate::sql::update::into_assignments(update)
        .map_err(|error| invalid(error.to_string()))?
        .into_iter()
        .map(|assignment| {
            let input = resolved.inputs.get(&assignment.field).ok_or_else(|| {
                invalid(format!(
                    "update column is not declared by the descriptor: {}",
                    assignment.field
                ))
            })?;
            let column = resolved.table.column(&input.column)?;
            let value = match assignment.operator {
                Operator::Set if assignment.operand.is_null() => Expression::Null,
                Operator::Set => {
                    Expression::Bind(registration.encode(input.storage, assignment.operand)?)
                }
                Operator::Increment | Operator::Decrement | Operator::Multiply => {
                    let operator = match assignment.operator {
                        Operator::Increment => ArithmeticOperator::Add,
                        Operator::Decrement => ArithmeticOperator::Subtract,
                        Operator::Multiply => ArithmeticOperator::Multiply,
                        _ => unreachable!(),
                    };
                    Expression::Arithmetic {
                        column: column.clone(),
                        operator,
                        operand: registration.encode(input.storage, assignment.operand)?,
                    }
                }
                Operator::Push | Operator::Pull | Operator::AddToSet => {
                    let operator = match assignment.operator {
                        Operator::Push => ArrayOperator::Push,
                        Operator::Pull => ArrayOperator::Pull,
                        Operator::AddToSet => ArrayOperator::AddToSet,
                        _ => unreachable!(),
                    };
                    Expression::ArrayMutation {
                        column: column.clone(),
                        operator,
                        operand: match assignment.operand {
                            Value::Json(encoded) => Value::Json(encoded),
                            value => Value::Json(value.to_string()),
                        },
                    }
                }
            };
            Ok(Assignment { column, value })
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    assignments.extend(resolve_generated(generated, resolved, registration)?);
    Ok(assignments)
}

pub(crate) fn resolve_generated(
    generated: &WriteAssignments,
    resolved: &ResolvedTable,
    registration: &SqlRegistration,
) -> Result<Vec<Assignment>, QueryError> {
    let mut assignments = Vec::with_capacity(generated.columns.len());
    for assignment in &generated.columns {
        let column = resolved.table.column(&assignment.column)?;
        let value = match &assignment.value {
            AssignedValue::CurrentTimestamp => Expression::CurrentTimestamp,
            AssignedValue::Increment(step) => Expression::Increment {
                column: column.clone(),
                step: *step,
            },
            AssignedValue::Bound(value) if value.is_null() => Expression::Null,
            AssignedValue::Bound(value) => {
                Expression::Bind(registration.encode(column.storage(), value.clone())?)
            }
        };
        assignments.push(Assignment { column, value });
    }
    Ok(assignments)
}

fn invalid(message: impl Into<String>) -> QueryError {
    QueryError::InvalidFilter(message.into())
}
