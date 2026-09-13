//! Catalog evidence and column-key access for the protection pipeline.
use super::*;
use crate::{error::DbError, protection::Protection};
use async_trait::async_trait;

#[async_trait(?Send)]
impl crate::protection::Catalog for PostgresBackend {
    async fn introspect_schema(
        &self,
        _app_id: &str,
        schema: &crate::sql::SchemaName,
        session: Option<&crate::driver::Session>,
    ) -> Result<crate::sql::catalog::LiveSchema, DbError> {
        let pooled;
        let client = if let Some(session) = session {
            session
                .get::<compio_postgres::PoolConnection>()
                .ok_or_else(|| DbError::internal("catalog received a non-PostgreSQL session"))?
        } else {
            pooled = self
                .pool()
                .acquire()
                .await
                .map_err(|error| pg_error::classify(&error))?;
            &pooled
        };
        pg_introspect::read_live_schema(client, schema.as_str())
            .await
            .map_err(pg_error::classify_schema_error)
    }
}
impl Protection for PostgresBackend {
    fn key_store(&self) -> &crate::encryption::KeyStore {
        self.key_store()
    }
}

impl PostgresBackend {
    pub async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError> {
        pg_introspect::estimate_row_count(self.pool(), app_id, collection)
            .await
            .map_err(pg_error::classify_schema_error)
    }
}
