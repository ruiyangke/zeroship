//! ORM search extensions, including dialect planning and protected projections.
use crate::{binding::DbBinding, driver::Session, error::DbError};
use async_trait::async_trait;
use zeroship_data_sql::{
    descriptors::{GeoPoint, VectorMetric},
    value::Value,
};

#[derive(Debug)]
pub struct VectorSearch<'a> {
    pub binding: &'a DbBinding,
    pub collection: &'a str,
    pub column: &'a str,
    pub query: &'a [f32],
    pub k: usize,
    pub metric: VectorMetric,
    pub filter: &'a Value,
    pub schema: &'a Value,
}
#[derive(Debug)]
pub struct SpatialSearch<'a> {
    pub binding: &'a DbBinding,
    pub collection: &'a str,
    pub column: &'a str,
    pub point: GeoPoint,
    pub radius_m: f64,
    pub filter: &'a Value,
    pub limit: Option<usize>,
    pub schema: &'a Value,
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
