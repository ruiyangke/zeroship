//! SQLite implementation of the ORM execution contracts.
use super::{
    SqliteBackend,
    session::{SqliteCancelHandle, SqliteSessionHandle},
};
use crate::{
    driver::*,
    error::*,
    storage::{SchemaIntrospect, SqlExecutor},
};
use async_trait::async_trait;
use zeroship_data_sql::{SchemaName, catalog::LiveSchema, compile::SqlDialect, value::Value};

#[derive(Debug)]
struct SqliteCancellation(SqliteCancelHandle);
#[async_trait(?Send)]
impl Cancellation for SqliteCancellation {
    async fn cancel(&self) -> Result<CancelDelivery, DbError> {
        let outcome = self.0.cancel().await?;
        let ack = match super::reservation::terminal_result(&outcome).0 {
            TerminalResult::RolledBack => CleanupAck::RolledBack,
            _ => CleanupAck::Indeterminate,
        };
        Ok(CancelDelivery::Settled(ack))
    }
}
#[async_trait(?Send)]
impl DriverSession for SqliteSessionHandle {
    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Value>, DbError> {
        self.query_typed(sql, params)
            .await
            .map(|rows| super::row_json::typed_rows_to_values(&rows))
    }
    async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        self.exec(sql, params).await
    }
    async fn settle(&self, intent: SettleIntent) -> (TerminalResult, Option<DbError>) {
        let intent = match intent {
            SettleIntent::Commit => super::session::TerminalIntent::Commit,
            SettleIntent::Rollback => super::session::TerminalIntent::Rollback,
        };
        match self.settle(intent).await {
            Ok(outcome) => super::reservation::terminal_result(&outcome),
            Err(error) => (TerminalResult::Indeterminate, Some(error)),
        }
    }
    async fn cleanup(&self) -> CleanupAck {
        super::reservation::cleanup(self).await
    }
    fn canceller(&self) -> Option<CancellationHandle> {
        self.cancel_handle()
            .map(|handle| CancellationHandle::new(SqliteCancellation(handle)))
    }
    fn discard(self: Box<Self>) {
        if let Err(error) = self.try_exec_detached("ROLLBACK", &[]) {
            tracing::warn!(%error, "withdrawing a SQLite session could not enqueue rollback");
        }
    }
}
#[async_trait(?Send)]
impl Driver for SqliteBackend {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }
    fn publishes_committed_changes(&self) -> bool {
        true
    }
    async fn prepare_for_app(&self, app_id: &str) -> Result<(), DbError> {
        self.attach_app_file(app_id).await
    }
    async fn query(
        &self,
        app_id: &str,
        _schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError> {
        self.attach_app_file(app_id).await?;
        self.query_values(sql, params).await
    }
    async fn open_tx_session(
        &self,
        app_id: &str,
        _schema: &SchemaName,
        _begin: BeginIntent,
    ) -> Result<Session, OpenSessionError> {
        self.attach_app_file(app_id).await?;
        let client = self.acquire_dedicated_client(app_id).await?;
        self.client_exec(&client, "BEGIN", &[]).await?;
        Ok(Session::new(client))
    }
}
#[async_trait(?Send)]
impl Catalog for SqliteBackend {
    async fn introspect_schema(&self, app_id: &str) -> Result<LiveSchema, DbError> {
        self.attach_app_file(app_id).await?;
        SchemaIntrospect::introspect_schema(self, app_id).await
    }
}
#[async_trait(?Send)]
impl PolicyStore for SqliteBackend {
    async fn persist_mask_policy(&self, app_id: &str, policy: &Value) -> Result<(), DbError> {
        super::mask_policy_store::persist(self, app_id, policy).await
    }
    async fn load_mask_policy(&self, app_id: &str) -> Result<Option<Value>, DbError> {
        super::mask_policy_store::load(self, app_id).await
    }
}
#[async_trait(?Send)]
impl Search for SqliteBackend {
    async fn vector_search(
        &self,
        session: Option<&Session>,
        r: VectorSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.attach_app_file(r.binding.app_id()).await?;
        let auto = self.autocommit_client();
        let session: &dyn DriverSession = session.map_or(&auto as &dyn DriverSession, |s| &**s);
        self.vector_search_on(
            session,
            r.binding,
            r.collection,
            r.column,
            r.query,
            r.k,
            r.metric,
            r.filter,
            r.schema,
        )
        .await
    }
    async fn spatial_near(
        &self,
        session: Option<&Session>,
        r: SpatialSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        self.attach_app_file(r.binding.app_id()).await?;
        let auto = self.autocommit_client();
        let session: &dyn DriverSession = session.map_or(&auto as &dyn DriverSession, |s| &**s);
        self.spatial_near_on(
            session,
            r.binding,
            r.collection,
            r.column,
            r.point,
            r.radius_m,
            r.filter,
            r.limit,
            r.schema,
        )
        .await
    }
}
impl Backend for SqliteBackend {
    fn key_store(&self) -> &crate::encryption::KeyStore {
        self.key_store()
    }
}
