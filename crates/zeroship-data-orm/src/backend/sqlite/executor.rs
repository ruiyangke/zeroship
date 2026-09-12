//! Host database routing and transaction authority setup.
use super::SqliteBackend;
use crate::sql::SchemaName;
use crate::value::Value;
use crate::{
    driver::{Driver, LeaseKind, Session},
    error::*,
    executor::ScopedExecutor,
};
use async_trait::async_trait;

#[async_trait(?Send)]
impl ScopedExecutor for SqliteBackend {
    fn namespace<'a>(&self, app_id: &'a str, _schema: &'a SchemaName) -> &'a str {
        app_id
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
        self.connection_driver(app_id)
            .await?
            .acquire(LeaseKind::Autocommit)
            .await?
            .query(sql, params)
            .await
    }
    async fn exec(
        &self,
        app_id: &str,
        _schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        self.connection_driver(app_id)
            .await?
            .acquire(LeaseKind::Autocommit)
            .await?
            .exec(sql, params)
            .await
    }
    async fn open_tx_session(
        &self,
        app_id: &str,
        _schema: &SchemaName,
        _begin: BeginIntent,
    ) -> Result<Session, OpenSessionError> {
        let session = self
            .connection_driver(app_id)
            .await?
            .acquire(LeaseKind::Transaction)
            .await?;
        session.exec("BEGIN", &[]).await?;
        Ok(session)
    }
}
impl SqliteBackend {
    /// Resolve and attach the app database before exposing a physical source.
    pub async fn connection_driver(
        &self,
        app_id: &str,
    ) -> Result<super::driver::SqliteDriver, DbError> {
        self.attach_app_file(app_id).await?;
        Ok(super::driver::SqliteDriver::new(
            self.session.clone(),
            app_id.to_owned(),
        ))
    }
}
