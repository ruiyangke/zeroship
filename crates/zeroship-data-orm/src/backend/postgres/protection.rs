//! Catalog evidence and column-key access for the protection pipeline.
use super::*;
use crate::{error::DbError, protection::Protection};
use async_trait::async_trait;

#[async_trait(?Send)]
impl crate::protection::Catalog for PostgresBackend {
    async fn introspect_schema(
        &self,
        app_id: &str,
    ) -> Result<crate::sql::catalog::LiveSchema, DbError> {
        // The PG-tier reader returns its local `SchemaError`; the sibling
        // translator re-creates the exact `coded_sql("diff: …", e)` shape, so
        // SQLSTATE classification and the operator-facing message stay
        // unchanged while the schema snapshot remains vendor-neutral.
        pg_introspect::read_live_schema(self.pool(), app_id)
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
