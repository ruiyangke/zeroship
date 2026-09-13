use super::resolved::ResolvedTable;
use crate::schema::{AssignmentEvent, AssignmentGenerator, FieldMap};
use crate::{
    error::DbError,
    sql::{registration::SqlRegistration, statement::IdentityRequest, SchemaName},
    value::Value,
};

pub(crate) fn is_generated(schema: &FieldMap) -> bool {
    schema
        .get("id")
        .and_then(|column| column.assignment.as_ref())
        .is_some_and(|assignment| {
            assignment.by == AssignmentGenerator::Identity
                && assignment.on == AssignmentEvent::Insert
        })
}

pub(super) fn requires_allocation(schema: &FieldMap, payload: &Value) -> bool {
    is_generated(schema)
        && payload.as_array().map_or_else(
            || super::write_pipeline::upsert_requires_conflict_probe(schema, payload),
            |documents| {
                documents
                    .iter()
                    .any(|doc| super::write_pipeline::upsert_requires_conflict_probe(schema, doc))
            },
        )
}

pub(super) fn request(
    namespace: &SchemaName,
    collection: &str,
    schema: &FieldMap,
    count: usize,
    registration: &SqlRegistration,
) -> Result<IdentityRequest, DbError> {
    if count == 0 || count > crate::budgets::MAX_INSERT_MANY_BATCH {
        return Err(DbError::validation(
            "generated_identity_batch",
            "identity allocation exceeds the insert batch limit",
        ));
    }
    let resolved = ResolvedTable::new(namespace, collection, schema, registration)?;
    let input = resolved.inputs.get("id").ok_or_else(|| {
        DbError::internal("generated identity descriptor has no resolved id column")
    })?;
    let column = resolved
        .table
        .column(&input.column)
        .map_err(crate::sql::mapping::QueryError::from)?;
    IdentityRequest::new(resolved.table.clone(), column, count)
        .map_err(crate::sql::mapping::QueryError::from)
        .map_err(Into::into)
}
