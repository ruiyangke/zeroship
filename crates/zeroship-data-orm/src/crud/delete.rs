use super::{predicate, resolved::ResolvedTable, update};
use crate::{
    sql::{
        mapping::QueryError,
        lifecycle::WriteAssignments,
        registration::SqlRegistration,
        statement::{
            Delete, DeleteParts, MutationScope, ResolvedOperand, ResolvedPredicate, Statement,
            Update, UpdateParts,
        },
        SchemaName,
    },
    value::Value,
};

pub(crate) fn build_hard(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    filter: predicate::Input,
    first: bool,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    let resolved = ResolvedTable::new(namespace, collection, schema, registration)?;
    let predicate = filter.resolve(schema, &resolved, registration)?;
    let scope = scope(first, schema, &resolved)?;
    let returning = if first {
        resolved.returning(schema)?
    } else {
        Vec::new()
    };
    registration
        .compile(Statement::Delete(Delete::new(DeleteParts {
            table: resolved.table,
            predicate,
            scope,
            returning,
        })?))
        .map_err(Into::into)
}

pub(crate) fn build_lifecycle(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    filter: predicate::Input,
    marker: &str,
    match_deleted: bool,
    generated: &WriteAssignments,
    first: bool,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    let resolved = ResolvedTable::new(namespace, collection, schema, registration)?;
    let marker = resolved
        .table
        .column(&crate::sql::mapping::value_column_for_field(marker, schema))?;
    let predicate = ResolvedPredicate::and(vec![
        filter.resolve(schema, &resolved, registration)?,
        ResolvedPredicate::IsNull {
            operand: ResolvedOperand::Column(marker),
            negated: match_deleted,
        },
    ]);
    let assignments = update::resolve_generated(generated, &resolved, registration)?;
    let scope = scope(first, schema, &resolved)?;
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

fn scope(
    first: bool,
    schema: &Value,
    resolved: &ResolvedTable,
) -> Result<MutationScope, QueryError> {
    Ok(if first {
        MutationScope::First {
            target: resolved
                .table
                .column(&crate::sql::mapping::value_column_for_field("id", schema))?,
        }
    } else {
        MutationScope::Matching
    })
}
