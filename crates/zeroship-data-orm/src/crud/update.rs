use super::{predicate, resolved::ResolvedTable};
use crate::{
    sql::{
        lifecycle::{AssignedValue, WriteAssignments},
        mapping::QueryError,
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
                .column(&crate::sql::mapping::value_column_for_field("id", schema))?,
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
                        operand: registration.encode(input.storage, assignment.operand)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{
        compiler::{CompileError, SqlCompiler, SqliteCompiler},
        registration::{SqlStorageCodecs, SQLITE_FAMILY},
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
    fn array_mutation_operands_cross_the_registered_codec_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let compiler = SqliteCompiler;
        let registration = SqlRegistration::new(
            "array-codec",
            SQLITE_FAMILY,
            compiler,
            CountingCodecs(calls.clone()),
            compiler.support(),
        )
        .unwrap();
        let schema = crate::value!({
            "id":{"type":"string","primaryKey":true},
            "items":{"type":"array","items":"json"}
        });
        let resolved = ResolvedTable::new(
            &SchemaName::new("app").unwrap(),
            "entries",
            &schema,
            &registration,
        )
        .unwrap();

        resolve_assignments(
            crate::value!({"items":{"$push":{"key":"value"}}}),
            &WriteAssignments::default(),
            &resolved,
            &registration,
        )
        .unwrap();

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
