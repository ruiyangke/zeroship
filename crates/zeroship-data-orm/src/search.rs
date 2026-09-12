//! ORM search extensions, including dialect planning and protected projections.
use crate::{binding::DbBinding, driver::Session, error::DbError, sql::compiler::CompiledQuery};
use crate::{sql::descriptors::GeoPoint, value::Value};
use async_trait::async_trait;

#[derive(Debug)]
pub struct VectorSearch<'a> {
    pub binding: &'a DbBinding,
    pub query: CompiledQuery,
}
impl<'a> VectorSearch<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn compile(
        binding: &'a DbBinding,
        collection: &str,
        column: &str,
        query: &[f32],
        limit: usize,
        metric: crate::sql::descriptors::VectorMetric,
        filter: &Value,
        schema: &Value,
        registration: &crate::sql::registration::SqlRegistration,
    ) -> Result<Self, DbError> {
        Ok(Self {
            binding,
            query: crate::crud::search::vector(
                binding.schema(),
                collection,
                schema,
                column,
                query.to_vec(),
                limit,
                metric,
                filter.clone(),
                registration,
            )?,
        })
    }
}
#[derive(Debug)]
pub struct SpatialSearch<'a> {
    pub binding: &'a DbBinding,
    pub query: CompiledQuery,
    pub column: &'a str,
    pub point: GeoPoint,
    pub radius_m: f64,
    pub limit: Option<usize>,
}
impl<'a> SpatialSearch<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn compile(
        binding: &'a DbBinding,
        collection: &str,
        column: &'a str,
        point: GeoPoint,
        radius_m: f64,
        filter: &Value,
        limit: Option<usize>,
        schema: &Value,
        registration: &crate::sql::registration::SqlRegistration,
    ) -> Result<Self, DbError> {
        Ok(Self {
            binding,
            query: crate::crud::search::spatial(
                binding.schema(),
                collection,
                schema,
                column,
                point,
                radius_m,
                filter.clone(),
                limit,
                registration,
            )?,
            column,
            point,
            radius_m,
            limit,
        })
    }
}

/// Database-specific search extensions must use the supplied transaction session.
#[async_trait(?Send)]
pub trait Search {
    async fn vector_search(
        &self,
        session: Option<&Session>,
        request: VectorSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        let _ = (session, request);
        Err(DbError::config(
            "backend_unsupported",
            "the backend does not support vector search",
        ))
    }
    async fn spatial_near(
        &self,
        session: Option<&Session>,
        request: SpatialSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        let _ = (session, request);
        Err(DbError::config(
            "backend_unsupported",
            "the backend does not support spatial search",
        ))
    }
}
