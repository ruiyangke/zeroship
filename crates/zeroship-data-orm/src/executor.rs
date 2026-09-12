//! Tenant routing and authority setup on the connection that executes the work.
use crate::{
    driver::Session,
    error::{BeginIntent, DbError, OpenSessionError},
};
use async_trait::async_trait;
use std::{any::Any, fmt::Debug};
use crate::value::Value;
use crate::sql::{SchemaName, compile::SqlDialect};

/// Host routing and authority setup above the physical connection driver.
#[async_trait(?Send)]
pub trait ScopedExecutor: Any + Debug {
    /// Physical SQL namespace selected by this host's routing strategy.
    fn namespace<'a>(&self, _app_id: &'a str, schema: &'a SchemaName) -> &'a str {
        schema.as_str()
    }

    fn dialect(&self) -> SqlDialect;
    fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        None
    }
    async fn prepare_for_app(&self, app_id: &str) -> Result<(), DbError>;
    /// Execute with the binding's authority, outside an explicit transaction.
    async fn query(
        &self,
        app_id: &str,
        schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError>;
    /// Execute with the binding's authority and return the affected-row count.
    async fn exec(
        &self,
        app_id: &str,
        schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError>;
    /// Return only after BEGIN and session authority setup have succeeded.
    async fn open_tx_session(
        &self,
        app_id: &str,
        schema: &SchemaName,
        begin: BeginIntent,
    ) -> Result<Session, OpenSessionError>;
}
