//! SQLite implementation of the ORM execution contracts.
use super::session::{SqliteCancelHandle, SqliteSessionHandle};
use crate::value::Value;
use crate::{driver::*, error::*};
use async_trait::async_trait;
use std::rc::Rc;

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
            .and_then(|rows| super::row_json::typed_rows_to_values(&rows))
    }
    async fn exec(&self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
        self.exec_values(sql, params).await
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
/// SQLite connection source for a host-selected database namespace.
pub struct SqliteDriver {
    session: Rc<super::session::SqliteSession>,
    namespace: String,
}
impl std::fmt::Debug for SqliteDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteDriver").finish_non_exhaustive()
    }
}
impl SqliteDriver {
    pub(super) fn new(session: Rc<super::session::SqliteSession>, namespace: String) -> Self {
        Self { session, namespace }
    }
}
#[async_trait(?Send)]
impl Driver for SqliteDriver {
    async fn acquire(&self, kind: LeaseKind) -> Result<Session, DbError> {
        let handle = match kind {
            LeaseKind::Autocommit => SqliteSessionHandle::new(self.session.clone()),
            LeaseKind::Transaction => {
                let lease = self.session.reserve_transaction(&self.namespace).await?;
                SqliteSessionHandle::with_lease(self.session.clone(), lease)
            }
        };
        Ok(Session::new(handle))
    }
}

#[cfg(test)]
mod tests {
    #[compio::test]
    async fn owned_driver_outlives_its_host() {
        use crate::driver::{Driver, LeaseKind};
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("driver.sqlite");
        let backend = super::super::SqliteBackend::open(
            &database_path,
            std::sync::Arc::new(super::super::NullChangeSink),
            crate::encryption::ProjectKeySource::unavailable(),
        )
        .await
        .unwrap();
        let driver = backend
            .connection_driver(
                "app_owned_driver",
                &crate::sql::SchemaName::new("app_owned_driver").unwrap(),
            )
            .await
            .unwrap();
        drop(backend);
        assert!(database_path.exists(), "the driver still owns the database");
        let session = driver.acquire(LeaseKind::Transaction).await.unwrap();
        drop(driver);
        assert!(
            database_path.exists(),
            "the session still owns the database"
        );
        session.exec("BEGIN", &[]).await.unwrap();
        session
            .exec("CREATE TABLE app_owned_driver.leased (id INTEGER)", &[])
            .await
            .unwrap();
        assert_eq!(
            session
                .exec(
                    "INSERT INTO app_owned_driver.leased VALUES ($1)",
                    &[1.into()]
                )
                .await
                .unwrap(),
            1
        );
        let (terminal, error) = session.settle(crate::error::SettleIntent::Commit).await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(terminal, crate::error::TerminalResult::Committed);
        drop(session);
        assert!(
            database_path.exists(),
            "closing a driver must preserve the database file"
        );
        let reopened = super::super::SqliteBackend::open(
            &database_path,
            std::sync::Arc::new(super::super::NullChangeSink),
            crate::encryption::ProjectKeySource::unavailable(),
        )
        .await
        .unwrap();
        let source = reopened
            .connection_driver(
                "app_owned_driver",
                &crate::sql::SchemaName::new("app_owned_driver").unwrap(),
            )
            .await
            .unwrap();
        let session = source.acquire(LeaseKind::Autocommit).await.unwrap();
        let rows = session
            .query("SELECT id FROM app_owned_driver.leased", &[])
            .await
            .unwrap();
        assert_eq!(rows[0]["id"], crate::value::Value::from(1));
    }

    #[compio::test]
    async fn native_exec_reports_command_counts() {
        let directory = tempfile::tempdir().unwrap();
        let backend = super::super::SqliteBackend::new(
            directory.path().to_owned(),
            std::sync::Arc::new(super::super::NullChangeSink),
            crate::encryption::ProjectKeySource::unavailable(),
        )
        .unwrap();
        let driver = backend
            .connection_driver(
                "app_driver",
                &crate::sql::SchemaName::new("app_driver").unwrap(),
            )
            .await
            .unwrap();
        crate::driver::tests::native_commands(driver, "BLOB").await;
    }

    #[compio::test]
    async fn invalid_native_results_are_errors_and_the_session_remains_usable() {
        use crate::driver::{Driver, LeaseKind};
        use crate::value::Value;
        let directory = tempfile::tempdir().unwrap();
        let backend = super::super::SqliteBackend::new(
            directory.path().to_owned(),
            std::sync::Arc::new(super::super::NullChangeSink),
            crate::encryption::ProjectKeySource::unavailable(),
        )
        .unwrap();
        let driver = backend
            .connection_driver(
                "app_decode",
                &crate::sql::SchemaName::new("app_decode").unwrap(),
            )
            .await
            .unwrap();
        let session = driver.acquire(LeaseKind::Autocommit).await.unwrap();
        for sql in [
            "SELECT 1e999 AS invalid_result",
            "SELECT -1e999 AS invalid_result",
        ] {
            let error = session.query(sql, &[]).await.expect_err(sql);
            assert!(error.to_string().contains("invalid_result"), "{error}");
        }
        let rows = session
            .query("SELECT 1 AS healthy, NULL AS absent", &[])
            .await
            .unwrap();
        assert_eq!(rows[0]["healthy"], Value::from(1));
        assert_eq!(rows[0]["absent"], Value::Null);
    }
}
