//! Host database routing and transaction authority setup.
use super::PostgresBackend;
use crate::binding::DbBinding;
use crate::value::Value;
use crate::{
    driver::{Driver, LeaseKind, Session},
    error::*,
    executor::ScopedExecutor,
};
use async_trait::async_trait;

#[async_trait(?Send)]
impl ScopedExecutor for PostgresBackend {
    fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        self.connection_driver().pool_counts()
    }
    async fn prepare_for_app(&self, _binding: &DbBinding) -> Result<(), DbError> {
        Ok(())
    }
    async fn query(
        &self,
        binding: &DbBinding,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError> {
        self.query_scoped_values(binding, sql, params).await
    }
    async fn exec(
        &self,
        binding: &DbBinding,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        super::pg_autocommit::scoped_execute(
            self.pool(),
            binding,
            self.session_authority(),
            sql,
            params,
        )
        .await
    }
    async fn check_connection(&self) -> Result<(), DbError> {
        // A pooled lease and a protocol Sync: no table, no authority setup and
        // no transaction lane, so an open transaction on this app's lane does
        // not delay the answer. The wait is the pool's acquire timeout.
        let client = self
            .pool()
            .acquire()
            .await
            .map_err(|error| super::pg_error::classify(&error))?;
        client
            .check_connection()
            .await
            .map_err(|error| super::pg_error::classify(&error))
    }
    async fn open_tx_session(
        &self,
        binding: &DbBinding,
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
        super::apply_session_authority(client, binding, self.session_authority()).await?;
        Ok(session)
    }
}
impl PostgresBackend {
    pub fn connection_driver(&self) -> super::driver::PostgresDriver {
        super::driver::PostgresDriver::new(self.pool().clone())
    }
}
