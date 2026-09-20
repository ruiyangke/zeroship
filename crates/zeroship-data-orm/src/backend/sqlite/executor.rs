//! Host database routing and transaction authority setup.
use super::SqliteBackend;
use crate::binding::DbBinding;
use crate::value::Value;
use crate::{
    driver::{Driver, LeaseKind, Session},
    error::*,
    executor::ScopedExecutor,
};
use async_trait::async_trait;

/// How an explicit creator transaction opens on SQLite.
///
/// **`IMMEDIATE` is load-bearing, not a style choice.** A bare `BEGIN` is
/// `BEGIN DEFERRED`: it takes no lock, so the transaction's first read pins a
/// snapshot and the first write has to *upgrade* to the write lock. SQLite
/// refuses that upgrade with the `SQLITE_BUSY` family and **does not invoke the
/// busy handler for it**, because a connection already holding a read
/// transaction cannot be made to wait without risking deadlock. The refusal is
/// therefore immediate, and the connection's `busy_timeout` - which
/// `session::lock_wait` sets from `budgets::DB_LOCK_TIMEOUT_MS`,
/// the same budget PostgreSQL spends on `lock_timeout` - buys nothing. The
/// second of two replicas racing over one file was refused outright rather than
/// waiting for its turn.
///
/// `IMMEDIATE` takes the write lock at `BEGIN`, where there is no prior read
/// transaction and the busy handler does run, so the lock budget applies to a
/// transaction the way it already applies to every other statement.
/// `enter_wal` in `session` documents the same SQLite rule for the WAL
/// switch; this is that rule applied to the transaction path.
///
/// The cost, stated because it is real: `IMMEDIATE` takes a write lock on every
/// database the connection has open, `main` included, not only the one this
/// binding addresses. A transaction lane carries `main` plus at most its own
/// app's attached file, so two apps on one backend now serialize their explicit
/// transactions through `main` instead of overlapping. They serialize rather
/// than deadlock: `main` is database zero, so every lane takes the locks in the
/// same order. This is an intentional divergence from PostgreSQL, and
/// `two_apps_on_one_backend_serialize_their_transactions_through_main` measures
/// it.
const BEGIN_TRANSACTION: &str = "BEGIN IMMEDIATE";

#[async_trait(?Send)]
impl ScopedExecutor for SqliteBackend {
    fn namespace<'a>(&self, binding: &'a DbBinding) -> &'a str {
        Self::database_alias(binding)
    }

    async fn prepare_for_app(&self, binding: &DbBinding) -> Result<(), DbError> {
        self.attach_binding(binding).await
    }
    async fn query(
        &self,
        binding: &DbBinding,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Value>, DbError> {
        self.connection_driver(binding)
            .await?
            .acquire(LeaseKind::Autocommit)
            .await?
            .query(sql, params)
            .await
    }
    async fn exec(
        &self,
        binding: &DbBinding,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        self.connection_driver(binding)
            .await?
            .acquire(LeaseKind::Autocommit)
            .await?
            .exec(sql, params)
            .await
    }
    async fn check_connection(&self) -> Result<(), DbError> {
        // One round trip to the actor on the autocommit connection. It does not
        // reserve a lane, so an app's open transaction does not delay it.
        self.autocommit_client().query("SELECT 1", &[]).await?;
        Ok(())
    }
    async fn open_tx_session(
        &self,
        binding: &DbBinding,
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
            .connection_driver(binding)
            .await?
            .acquire(LeaseKind::Transaction)
            .await?;
        session.exec(BEGIN_TRANSACTION, &[]).await?;
        Ok(session)
    }
}
impl SqliteBackend {
    /// Make the binding's database addressable before exposing a physical source.
    pub async fn connection_driver(
        &self,
        binding: &DbBinding,
    ) -> Result<super::driver::SqliteDriver, DbError> {
        self.attach_binding(binding).await?;
        // THE ALIAS, not the tenant. The actor uses this one string for the
        // transaction lane key, the lane's own ATTACH and the lookup that finds
        // which file to attach, and every one of those is about the DATABASE.
        // An attachment is registered under the alias, so a lane keyed on the
        // tenant would find no file and every qualified statement on a
        // transaction connection would report no such table.
        Ok(super::driver::SqliteDriver::new(
            self.session.clone(),
            Self::database_alias(binding).to_owned(),
        ))
    }
}
