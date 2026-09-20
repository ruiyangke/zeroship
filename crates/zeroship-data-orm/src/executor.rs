//! Tenant routing and authority setup on the connection that executes the work.
use crate::binding::DbBinding;
use crate::value::Value;
use crate::{
    driver::Session,
    error::{BeginIntent, DbError, OpenSessionError},
};
use async_trait::async_trait;
use std::{any::Any, fmt::Debug};

/// Host routing and authority setup above the physical connection driver.
///
/// Every method takes the whole [`DbBinding`] rather than a tenant and a
/// schema. The PostgreSQL host narrows to the binding's own role, the SQLite
/// host attaches the tenant's file, and both qualify statements with the
/// schema - three answers derived from one value, so no call site can pair a
/// tenant with another database's schema.
#[async_trait(?Send)]
pub trait ScopedExecutor: Any + Debug {
    /// Physical SQL namespace selected by this host's routing strategy.
    fn namespace<'a>(&self, binding: &'a DbBinding) -> &'a str {
        binding.schema().as_str()
    }

    fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        None
    }
    /// Make the binding's database addressable before a path that bypasses
    /// [`Self::query`] and [`Self::exec`] reaches it.
    async fn prepare_for_app(&self, binding: &DbBinding) -> Result<(), DbError>;
    /// Execute with the binding's authority, outside an explicit transaction.
    async fn query(
        &self,
        binding: &DbBinding,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError>;
    /// Execute with the binding's authority and return the affected-row count.
    async fn exec(&self, binding: &DbBinding, sql: &str, params: &[Value])
        -> Result<u64, DbError>;
    /// Confirm the connection source can answer, in one round trip on an
    /// autocommit lease. It claims no transaction lane, installs no authority
    /// and reads no table.
    async fn check_connection(&self) -> Result<(), DbError>;
    /// Return only after BEGIN and session authority setup have succeeded.
    async fn open_tx_session(
        &self,
        binding: &DbBinding,
        begin: BeginIntent,
    ) -> Result<Session, OpenSessionError>;
}
