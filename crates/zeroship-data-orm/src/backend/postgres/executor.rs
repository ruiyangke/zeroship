//! Host database routing and transaction authority setup.
use super::PostgresBackend;
use crate::{
    driver::{Driver, LeaseKind, Session},
    error::*,
    executor::ScopedExecutor,
};
use async_trait::async_trait;
use zeroship_data_sql::{SchemaName, compile::SqlDialect, value::Value};

#[async_trait(?Send)]
impl ScopedExecutor for PostgresBackend {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }
    fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        self.connection_driver().pool_counts()
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
    async fn exec(
        &self,
        _app_id: &str,
        schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        super::pg_autocommit::roled_execute(self.pool(), schema, sql, params).await
    }
    async fn open_tx_session(
        &self,
        _app_id: &str,
        schema: &SchemaName,
        begin: BeginIntent,
    ) -> Result<Session, OpenSessionError> {
        let session = self
            .connection_driver()
            .acquire(LeaseKind::Transaction)
            .await?;
        let client = session
            .get::<compio_postgres::PoolConnection>()
            .expect("PostgresDriver returns a PostgreSQL lease");
        client
            .batch_execute(&super::render_begin(begin))
            .await
            .map_err(|e| super::pg_error::classify(&e))?;
        super::apply_per_app_role(client, schema).await?;
        Ok(session)
    }
}
impl PostgresBackend {
    pub fn connection_driver(&self) -> super::driver::PostgresDriver {
        super::driver::PostgresDriver::new(self.pool().clone())
    }
}
