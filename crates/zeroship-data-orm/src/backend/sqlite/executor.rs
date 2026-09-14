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
    fn namespace<'a>(&self, app_id: &'a str, schema: &'a SchemaName) -> &'a str {
        Self::database_alias(app_id, schema)
    }

    async fn prepare_for_app(&self, app_id: &str, schema: &SchemaName) -> Result<(), DbError> {
        self.attach_binding(app_id, schema).await
    }
    async fn query(
        &self,
        app_id: &str,
        schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError> {
        self.connection_driver(app_id, schema)
            .await?
            .acquire(LeaseKind::Autocommit)
            .await?
            .query(sql, params)
            .await
    }
    async fn exec(
        &self,
        app_id: &str,
        schema: &SchemaName,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        self.connection_driver(app_id, schema)
            .await?
            .acquire(LeaseKind::Autocommit)
            .await?
            .exec(sql, params)
            .await
    }
    async fn open_tx_session(
        &self,
        app_id: &str,
        schema: &SchemaName,
        begin: BeginIntent,
    ) -> Result<Session, OpenSessionError> {
        if let BeginIntent::Isolation(level) = begin {
            if level != IsolationLevel::Serializable {
                return Err(DbError::validation(
                    "unsupported_isolation_level",
                    format!(
                        "db.transaction: SQLite supports only SERIALIZABLE isolation; requested {}",
                        level.ansi_name(),
                    ),
                )
                .into());
            }
        }
        let session = self
            .connection_driver(app_id, schema)
            .await?
            .acquire(LeaseKind::Transaction)
            .await?;
        session.exec("BEGIN", &[]).await?;
        Ok(session)
    }
}
impl SqliteBackend {
    /// Make the binding's database addressable before exposing a physical source.
    pub async fn connection_driver(
        &self,
        app_id: &str,
        schema: &SchemaName,
    ) -> Result<super::driver::SqliteDriver, DbError> {
        self.attach_binding(app_id, schema).await?;
        Ok(super::driver::SqliteDriver::new(
            self.session.clone(),
            app_id.to_owned(),
        ))
    }
}
