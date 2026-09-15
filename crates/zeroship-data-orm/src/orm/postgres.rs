//! `PostgreSQL` coordination for native Rust hosts: advisory locks and
//! transaction-local settings.
//!
//! The extension is part of the Rust `Database` API only. Its statements are
//! compiled by the handle's SQL registration. Transaction commands run on the
//! handle's captured route, so they share the pinned session, savepoint frames
//! and supervised cancellation of the transaction that issued them. A session
//! lease pins its own pooled session outside any transaction.

#![expect(
    clippy::future_not_send,
    reason = "ORM operations use thread-local compio sessions"
)]

use super::{check_scope, Database, DbError};
use crate::driver::Session;
use crate::error::{BeginIntent, OpenSessionError, SettleIntent, TerminalResult};
use crate::sql::compiler::{CompileError, CompiledQuery};
use crate::sql::coordination::{
    AdvisoryLock, AdvisoryLockAction, AdvisoryLockScope, SetTransactionSetting, SettingName,
    ADVISORY_ACQUIRED, ADVISORY_RELEASED,
};
use crate::sql::statement::Statement;
use crate::value::Value;
use std::future::Future;

pub use crate::sql::coordination::AdvisoryKey;

const ADVISORY_LOCKS: &str = "advisory transaction locks";
const SESSION_LEASES: &str = "advisory session leases";
const TRANSACTION_SETTINGS: &str = "transaction settings";

/// A validated custom setting name for [`Postgres::set_local`].
///
/// Names are two lowercase identifiers joined by a dot, such as
/// `app_ns.flag`. Built-in settings, which are all dotless, are refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionSetting(SettingName);

impl TransactionSetting {
    /// # Errors
    /// `invalid_transaction_setting` when the name is not a dotted pair of
    /// lowercase identifiers.
    pub fn new(name: &str) -> Result<Self, DbError> {
        SettingName::new(name)
            .map(Self)
            .map_err(|error| DbError::validation("invalid_transaction_setting", error.to_string()))
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.0.as_str()
    }
}

/// PostgreSQL-specific operations bound to one database handle.
#[derive(Clone, Debug)]
pub struct Postgres {
    database: Database,
}

impl Database {
    /// `PostgreSQL` coordination for this handle.
    ///
    /// # Errors
    /// `unsupported_backend_feature` when the handle is not bound to
    /// `PostgreSQL`, or `transaction_scope_expired` for a settled transaction
    /// handle.
    pub fn postgres(&self) -> Result<Postgres, DbError> {
        self.check_scope()?;
        if self.backend.sql_registration().family() != crate::sql::registration::POSTGRES_FAMILY {
            return Err(super::unsupported_backend_feature("the PostgreSQL extension"));
        }
        Ok(Postgres {
            database: self.clone(),
        })
    }
}

impl Postgres {
    /// Wait for a transaction-scoped advisory lock on `key`.
    ///
    /// The lock is held until the transaction commits or rolls back,
    /// including supervised cancellation. Acquiring it again in the same
    /// transaction stacks and still releases once at settlement. The wait is
    /// bounded by the transaction's lock timeout.
    ///
    /// # Errors
    /// `transaction_required` on a root handle, `lock_not_available` when the
    /// lock timeout expires, or the database failure. A failed wait aborts the
    /// transaction.
    pub fn advisory_xact_lock(
        &self,
        key: AdvisoryKey,
    ) -> impl Future<Output = Result<(), DbError>> + use<> {
        let command = self.transaction_command(ADVISORY_LOCKS, "invalid_advisory_key", || {
            AdvisoryLock::new(key, AdvisoryLockScope::Transaction, AdvisoryLockAction::Wait)
                .map(Statement::AdvisoryLock)
                .map_err(|error| coordination_error(ADVISORY_LOCKS, "invalid_advisory_key", error))
        });
        async move { command?.run().await }
    }

    /// Set a custom setting until the transaction or enclosing savepoint ends.
    ///
    /// Name and value are bound parameters of `set_config(name, value, true)`.
    /// The setting's namespace must have been declared with
    /// [`crate::ConnectOptions::transaction_setting_namespace`].
    ///
    /// # Errors
    /// `transaction_required` on a root handle, `invalid_transaction_setting`
    /// for an undeclared namespace or a value containing NUL, or the database
    /// failure.
    pub fn set_local(
        &self,
        setting: &TransactionSetting,
        value: &str,
    ) -> impl Future<Output = Result<(), DbError>> + use<> {
        let database = &self.database;
        let command = self.transaction_command(TRANSACTION_SETTINGS, "invalid_transaction_setting", || {
            let namespace = setting.0.namespace();
            if !database
                .backend
                .declares_transaction_setting_namespace(namespace)
            {
                return Err(DbError::validation_hinted(
                    "invalid_transaction_setting",
                    format!("transaction setting namespace '{namespace}' is not declared"),
                    "Declare the namespace with ConnectOptions::transaction_setting_namespace.",
                ));
            }
            SetTransactionSetting::new(setting.0.clone(), value)
                .map(Statement::SetTransactionSetting)
                .map_err(|error| {
                    coordination_error(TRANSACTION_SETTINGS, "invalid_transaction_setting", error)
                })
        });
        async move { command?.run().await }
    }

    /// Try once to take a session-scoped advisory lock on `key`.
    ///
    /// On success the lease pins a pooled session holding the key across any
    /// number of later transactions. `Ok(None)` means another session holds
    /// it. Acquisition runs in a short transaction carrying the backend's
    /// authority and limits; session locks survive its commit.
    ///
    /// # Errors
    /// `session_lease_requires_root` on a transaction handle, or the database
    /// failure. A session in an uncertain state is discarded.
    pub fn try_session_lease(
        &self,
        key: AdvisoryKey,
    ) -> impl Future<Output = Result<Option<SessionLease>, DbError>> + use<> {
        let database = &self.database;
        let request = database.context.with(|| {
            database.check_scope()?;
            if database.scope.is_some() {
                return Err(DbError::validation_hinted(
                    "session_lease_requires_root",
                    "advisory session leases require a root database handle",
                    "A lease outlives transactions. Take it from a handle outside any \
                     transaction callback.",
                ));
            }
            let registration = database.backend.sql_registration();
            let compile = |key: AdvisoryKey, action| {
                AdvisoryLock::new(key, AdvisoryLockScope::Session, action)
                    .and_then(|lock| registration.compile(Statement::AdvisoryLock(lock)))
                    .map_err(|error| {
                        coordination_error(SESSION_LEASES, "invalid_advisory_key", error)
                    })
            };
            Ok(LeaseRequest {
                context: database.context.clone(),
                backend: database.backend.clone(),
                app_id: database.binding.app_id().to_owned(),
                schema: database.binding.schema().clone(),
                acquire: compile(key.clone(), AdvisoryLockAction::Try)?,
                release: compile(key, AdvisoryLockAction::Release)?,
            })
        });
        async move { request?.acquire().await }
    }

    fn transaction_command(
        &self,
        feature: &'static str,
        invalid_code: &'static str,
        statement: impl FnOnce() -> Result<Statement, DbError>,
    ) -> Result<TransactionCommand, DbError> {
        let database = &self.database;
        database.context.with(|| {
            database.check_scope()?;
            let route = database.capture_route();
            if database.scope.is_none() || !route.in_tx() {
                return Err(super::transaction_required(feature));
            }
            let query = route
                .sql_registration()
                .compile(statement()?)
                .map_err(|error| coordination_error(feature, invalid_code, error))?;
            Ok(TransactionCommand {
                context: database.context.clone(),
                backend: database.backend.clone(),
                scope: database.scope.clone(),
                route,
                query,
            })
        })
    }
}

/// A compiled coordination statement with its transaction route captured.
struct TransactionCommand {
    context: crate::OrmContext,
    backend: crate::backend::BackendHandle,
    scope: Option<std::rc::Rc<std::cell::Cell<bool>>>,
    route: crate::tx_route::CapturedRoute,
    query: CompiledQuery,
}

impl TransactionCommand {
    async fn run(self) -> Result<(), DbError> {
        check_scope(self.scope.as_ref())?;
        let Self {
            context,
            backend,
            route,
            query,
            ..
        } = self;
        context
            .scope(async move {
                let route = route.bind(backend)?;
                crate::exec::run_statement(&route, query.sql(), query.params()).await?;
                Ok(())
            })
            .await
    }
}

struct LeaseRequest {
    context: crate::OrmContext,
    backend: crate::backend::BackendHandle,
    app_id: String,
    schema: crate::sql::SchemaName,
    acquire: CompiledQuery,
    release: CompiledQuery,
}

impl LeaseRequest {
    async fn acquire(self) -> Result<Option<SessionLease>, DbError> {
        let Self {
            context,
            backend,
            app_id,
            schema,
            acquire,
            release,
        } = self;
        context
            .scope(async move {
                let session = backend
                    .open_tx_session(&app_id, &schema, BeginIntent::Default)
                    .await
                    .map_err(|error| match error {
                        OpenSessionError::Failed(error) => error,
                        OpenSessionError::Setup(error) => error.into_db_error(),
                    })?;
                let session = PinnedSession(Some(session));
                let rows = session.get().query(acquire.sql(), acquire.params()).await?;
                let acquired = flag(&rows, ADVISORY_ACQUIRED)?;
                let (terminal, error) = session.get().settle(SettleIntent::Commit).await;
                if terminal != TerminalResult::Committed {
                    return Err(error.unwrap_or_else(|| {
                        DbError::internal("advisory session lease acquisition did not commit")
                    }));
                }
                if !acquired {
                    session.return_to_pool();
                    return Ok(None);
                }
                Ok(Some(SessionLease {
                    session: Some(session.into_inner()),
                    release,
                }))
            })
            .await
    }
}

/// A session-scoped advisory lock held on a pinned pooled session.
///
/// [`Self::release`] unlocks and returns the session to its pool. Dropping an
/// unreleased lease discards the session instead, so the server frees the
/// key when the connection closes and no pooled session inherits it.
#[must_use = "a session lease holds its key until released or dropped"]
pub struct SessionLease {
    session: Option<Session>,
    release: CompiledQuery,
}

impl SessionLease {
    /// Release the key and return the session to its pool.
    ///
    /// # Errors
    /// The database failure, or `internal` when the server reports the key
    /// was not held. Either way the session is discarded rather than reused.
    pub async fn release(mut self) -> Result<(), DbError> {
        let session = PinnedSession(self.session.take());
        let rows = session
            .get()
            .query(self.release.sql(), self.release.params())
            .await?;
        if flag(&rows, ADVISORY_RELEASED)? {
            session.return_to_pool();
            Ok(())
        } else {
            Err(DbError::internal(
                "the advisory session lease no longer held its key",
            ))
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            session.discard();
        }
    }
}

impl std::fmt::Debug for SessionLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLease").finish_non_exhaustive()
    }
}

/// A session discarded on drop unless explicitly returned or kept.
struct PinnedSession(Option<Session>);

impl PinnedSession {
    const fn get(&self) -> &Session {
        self.0.as_ref().expect("a pinned session is present until consumed")
    }

    /// Return a session whose state was confirmed to its driver's pool.
    fn return_to_pool(mut self) {
        drop(self.0.take());
    }

    fn into_inner(mut self) -> Session {
        self.0.take().expect("a pinned session is present until consumed")
    }
}

impl Drop for PinnedSession {
    fn drop(&mut self) {
        if let Some(session) = self.0.take() {
            session.discard();
        }
    }
}

fn flag(rows: &[Value], column: &str) -> Result<bool, DbError> {
    match rows.first().and_then(|row| row.get(column)) {
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(DbError::internal(format!(
            "advisory lock statement did not return '{column}'"
        ))),
    }
}

fn coordination_error(feature: &str, invalid_code: &'static str, error: CompileError) -> DbError {
    match error {
        CompileError::Unsupported(_) => super::unsupported_backend_feature(feature),
        CompileError::InvalidStatement(message) => DbError::validation(invalid_code, message),
        error @ CompileError::BindLimitExceeded { .. } => DbError::internal(error.to_string()),
        // Advisory-lock and session coordination statements carry no instant,
        // so a precision refusal from this compiler would be a bug here.
        error @ CompileError::TimestampPrecisionUnsupported => {
            DbError::internal(error.to_string())
        }
    }
}
