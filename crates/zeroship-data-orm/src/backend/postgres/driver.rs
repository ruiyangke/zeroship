//! PostgreSQL implementation of the ORM execution contracts.
use super::PostgresBackend;
use crate::{
    driver::*,
    error::*,
    storage::{SchemaIntrospect, SqlExecutor},
};
use async_trait::async_trait;
use compio_postgres::{CancelToken, Pool, PoolConnection};
use zeroship_data_sql::{SchemaName, catalog::LiveSchema, compile::SqlDialect, value::Value};

#[derive(Debug)]
struct PgCancellation {
    pool: Pool,
    token: CancelToken,
}
#[async_trait(?Send)]
impl Cancellation for PgCancellation {
    async fn cancel(&self) -> Result<CancelDelivery, DbError> {
        self.pool.cancel_query(&self.token).await.map_err(|error| {
            DbError::internal(format!(
                "db.transaction: could not deliver a cancellation request: {error}"
            ))
        })?;
        Ok(CancelDelivery::Requested)
    }
}

#[async_trait(?Send)]
impl DriverSession for PoolConnection {
    fn server_process_id(&self) -> Option<i32> {
        Some(self.process_id())
    }
    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Value>, DbError> {
        super::params::query(self, sql, params)
            .await
            .map(|rows| super::pg_row_json::rows_to_values(&rows))
    }
    async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        self.query_text_params(sql, params)
            .await
            .map(|rows| rows.len() as u64)
            .map_err(|e| super::pg_error::classify(&e))
    }
    async fn settle(&self, intent: SettleIntent) -> (TerminalResult, Option<DbError>) {
        match self.batch_execute_reporting_tag(intent.verb()).await {
            Ok(tag) => (super::terminal_from_tag(intent, tag.as_deref()), None),
            Err(error) => (
                super::terminal_from_status(self.transaction_status()),
                Some(super::pg_error::classify(&error)),
            ),
        }
    }
    async fn cleanup(&self) -> CleanupAck {
        super::cleanup(self).await
    }
    fn canceller(&self) -> Option<CancellationHandle> {
        Some(CancellationHandle::new(PgCancellation {
            pool: self.pool().clone(),
            token: self.cancel_token(),
        }))
    }
    fn discard(self: Box<Self>) {
        PoolConnection::discard(*self);
    }
}

#[async_trait(?Send)]
impl Driver for PostgresBackend {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }
    fn publishes_committed_changes(&self) -> bool {
        false
    }
    fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        let pool = self.pool();
        Some((pool.idle_count(), pool.active_count(), pool.total_count()))
    }
    async fn prepare_for_app(&self, _app_id: &str) -> Result<(), DbError> {
        Ok(())
    }
    async fn query(
        &self,
        _app_id: &str,
        schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError> {
        self.query_roled_values(schema, sql, params).await
    }
    async fn open_tx_session(
        &self,
        app_id: &str,
        schema: &SchemaName,
        begin: BeginIntent,
    ) -> Result<Session, OpenSessionError> {
        let client = self.acquire_dedicated_client(app_id).await?;
        self.client_exec(&client, &super::render_begin(begin), &[])
            .await?;
        super::apply_per_app_role(&client, schema).await?;
        Ok(Session::new(client))
    }
}
#[async_trait(?Send)]
impl Catalog for PostgresBackend {
    async fn introspect_schema(&self, app_id: &str) -> Result<LiveSchema, DbError> {
        SchemaIntrospect::introspect_schema(self, app_id).await
    }
}
#[async_trait(?Send)]
impl PolicyStore for PostgresBackend {
    async fn persist_mask_policy(&self, _app_id: &str, _policy: &Value) -> Result<(), DbError> {
        Ok(())
    }
    async fn load_mask_policy(&self, _app_id: &str) -> Result<Option<Value>, DbError> {
        Ok(None)
    }
}
#[async_trait(?Send)]
impl Search for PostgresBackend {
    async fn vector_search(
        &self,
        session: Option<&Session>,
        r: VectorSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        let q = self
            .plan_vector_search(
                r.binding,
                r.collection,
                r.column,
                r.query,
                r.k,
                r.metric,
                r.filter,
                r.schema,
            )
            .await?;
        match session {
            Some(session) => session.query(&q.sql, &q.params).await,
            None => {
                Driver::query(
                    self,
                    r.binding.app_id(),
                    r.binding.schema(),
                    &q.sql,
                    &q.params,
                )
                .await
            }
        }
    }
    async fn spatial_near(
        &self,
        session: Option<&Session>,
        r: SpatialSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        let q = self
            .plan_spatial_near(
                r.binding,
                r.collection,
                r.column,
                r.point,
                r.radius_m,
                r.filter,
                r.limit,
                r.schema,
            )
            .await?;
        match session {
            Some(session) => session.query(&q.sql, &q.params).await,
            None => {
                Driver::query(
                    self,
                    r.binding.app_id(),
                    r.binding.schema(),
                    &q.sql,
                    &q.params,
                )
                .await
            }
        }
    }
}
impl Backend for PostgresBackend {
    fn key_store(&self) -> &crate::encryption::KeyStore {
        self.key_store()
    }
}
