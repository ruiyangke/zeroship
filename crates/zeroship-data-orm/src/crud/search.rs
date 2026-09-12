use super::{predicate, resolved::ResolvedTable};
use crate::{
    sql::{
        compile::{self, QueryError},
        compiler::CompiledQuery,
        descriptors::{GeoPoint, VectorMetric},
        registration::SqlRegistration,
        statement::{
            SpatialNearParts, SpatialNearStatement, Statement, VectorSearchParts,
            VectorSearchStatement,
        },
        SchemaName,
    },
    value::Value,
};

const MAX_SEARCH_LIMIT: usize = 500;

#[allow(clippy::too_many_arguments)]
pub(crate) fn vector(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    field: &str,
    query: Vec<f32>,
    limit: usize,
    metric: VectorMetric,
    filter: Value,
    registration: &SqlRegistration,
) -> Result<CompiledQuery, crate::error::DbError> {
    if metric == VectorMetric::InnerProduct && !registration.support().inner_product_vector_search {
        return Err(crate::error::DbError::config_hinted(
            "vector_unsupported_metric",
            "the registered SQL backend does not support inner-product vector search",
            "use cosine or L2 distance for this backend",
        ));
    }
    vector_query(
        namespace,
        collection,
        schema,
        field,
        query,
        limit,
        metric,
        filter,
        registration,
    )
    .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
fn vector_query(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    field: &str,
    query: Vec<f32>,
    limit: usize,
    metric: VectorMetric,
    filter: Value,
    registration: &SqlRegistration,
) -> Result<CompiledQuery, QueryError> {
    validate_read_field(field, schema)?;
    validate_limit("search.k", limit)?;
    let resolved = ResolvedTable::aliased(namespace, collection, "source", schema, registration)?;
    let input = resolved
        .inputs
        .get(field)
        .ok_or_else(|| invalid(format!("search field has no physical column: {field}")))?;
    let vector = resolved.table.column(&input.column)?;
    let mut query = Value::Array(
        query
            .into_iter()
            .map(|value| {
                Value::try_from(f64::from(value))
                    .map_err(|_| invalid("vector query contains a non-finite component"))
            })
            .collect::<Result<_, _>>()?,
    );
    crate::sql::codecs::prepare_value(field, &schema[field], &mut query)
        .map_err(|error| invalid(error.to_string()))?;
    let query = registration.encode(input.storage, query)?;
    let predicate = predicate::Input::Dynamic(filter).resolve(schema, &resolved, registration)?;
    let identity = resolved
        .inputs
        .get("id")
        .ok_or_else(|| invalid("collection descriptor omitted id"))?;
    let statement = Statement::VectorSearch(VectorSearchStatement::new(VectorSearchParts {
        projection: resolved.returning(schema)?,
        identity: resolved.table.column(&identity.column)?,
        vector,
        query,
        metric,
        predicate,
        limit: i64::try_from(limit).map_err(|_| invalid("search limit exceeds integer range"))?,
        table: resolved.table,
    })?);
    registration.compile(statement).map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spatial(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    field: &str,
    point: GeoPoint,
    radius_m: f64,
    filter: Value,
    limit: Option<usize>,
    registration: &SqlRegistration,
) -> Result<CompiledQuery, crate::error::DbError> {
    spatial_query(
        namespace,
        collection,
        schema,
        field,
        point,
        radius_m,
        filter,
        limit,
        registration,
    )
    .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
fn spatial_query(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    field: &str,
    point: GeoPoint,
    radius_m: f64,
    filter: Value,
    limit: Option<usize>,
    registration: &SqlRegistration,
) -> Result<CompiledQuery, QueryError> {
    validate_read_field(field, schema)?;
    let limit = limit.unwrap_or(100);
    validate_limit("near.limit", limit)?;
    let resolved = ResolvedTable::aliased(namespace, collection, "source", schema, registration)?;
    let input = resolved
        .inputs
        .get(field)
        .ok_or_else(|| invalid(format!("spatial field has no physical column: {field}")))?;
    let spatial = resolved.table.column(&input.column)?;
    let point = registration.encode(
        input.storage,
        crate::value!({"lat":point.lat,"lng":point.lng}),
    )?;
    let predicate = predicate::Input::Dynamic(filter).resolve(schema, &resolved, registration)?;
    let statement = Statement::SpatialNear(SpatialNearStatement::new(SpatialNearParts {
        projection: resolved.returning(schema)?,
        spatial,
        point,
        radius_m,
        predicate,
        limit: i64::try_from(limit).map_err(|_| invalid("near limit exceeds integer range"))?,
        table: resolved.table,
    })?);
    registration.compile(statement).map_err(Into::into)
}

fn validate_limit(name: &str, value: usize) -> Result<(), QueryError> {
    if value > MAX_SEARCH_LIMIT {
        return Err(invalid(format!(
            "{name} exceeds the maximum of {}, got {value}",
            MAX_SEARCH_LIMIT
        )));
    }
    Ok(())
}

fn validate_read_field(field: &str, schema: &Value) -> Result<(), QueryError> {
    compile::validate_field_name(field)?;
    if crate::sql::descriptors::readable_fields(schema).contains(field) {
        Ok(())
    } else {
        Err(QueryError::InvalidIdent(format!(
            "field '{field}' is not a readable schema field"
        )))
    }
}

fn invalid(message: impl Into<String>) -> QueryError {
    QueryError::InvalidFilter(message.into())
}
