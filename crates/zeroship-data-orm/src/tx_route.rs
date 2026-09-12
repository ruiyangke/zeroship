//! Per-dispatch SQL routing, captured before asynchronous backend resolution.
//!
//! A transaction belongs to a callback's async context. App identity alone
//! cannot distinguish an orphan from work in a replacement transaction. The
//! captured scope therefore carries the session generation and savepoint frame.
//! Execution validates both before touching the lane. Unrelated callbacks use
//! the pool even while the app has a transaction open.
//!
//! Capture and bind are separate because hosts observe async context
//! synchronously, while opening a connection can yield. The route also carries
//! the physical schema and immutable SQL registration used by query preparation.

use crate::backend::BackendHandle;
use crate::sql::registration::SqlRegistration;
use crate::sql::SchemaName;
use crate::transaction::scope::TransactionScope;

/// A synchronous routing decision awaiting backend binding.
/// Capture callback identity before asynchronous connection setup can yield.
#[derive(Debug)]
pub struct CapturedRoute {
    app_id: String,
    /// Physical schema used for SQL qualification and PostgreSQL role selection.
    /// App identity remains the transaction-lane and metering key.
    schema: SchemaName,
    /// `true` iff this dispatch is lexically-and-asynchronously inside a
    /// `db.transaction(fn)` callback **for this same app**.
    in_tx: bool,
    scope: Option<TransactionScope>,
    /// Compiler, codecs, and effective support captured before backend acquisition.
    registration: SqlRegistration,
    connection: Option<crate::connection::ConnectionIdentity>,
}

/// A captured dispatch bound to its backend. It carries app identity, callback
/// scope and SQL registration together, so execution does not re-read host context.
/// Construct through [`CapturedRoute::bind`].
#[derive(Debug)]
pub struct TxRoute {
    app_id: String,
    schema: SchemaName,
    in_tx: bool,
    scope: Option<TransactionScope>,
    backend: BackendHandle,
    registration: SqlRegistration,
    connection: Option<crate::connection::ConnectionIdentity>,
}

impl CapturedRoute {
    /// Freeze the host's observed async scope for this dispatch. A scope for
    /// another app does not confer access to this app's transaction.
    pub fn capture(
        current_scope: Option<&TransactionScope>,
        app_id: &str,
        schema: SchemaName,
        registration: SqlRegistration,
        connection: Option<crate::connection::ConnectionIdentity>,
    ) -> Self {
        // SEC-1 compares TENANT against TENANT. The schema rides along; it is
        // never the admission key, because two apps sharing one database would
        // share a schema and must still not share a transaction frame.
        let scope = current_scope
            .filter(|scope| scope.app_id() == app_id)
            .cloned();
        let in_tx = scope.is_some();
        Self {
            app_id: app_id.to_string(),
            schema,
            in_tx,
            scope,
            registration,
            connection,
        }
    }

    /// The physical schema this dispatch qualifies its tables with.
    pub fn schema(&self) -> &SchemaName {
        &self.schema
    }

    /// App identity used for transaction lanes and metering.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Whether the captured dispatch belongs to a transaction callback.
    pub fn in_tx(&self) -> bool {
        self.in_tx
    }

    pub fn sql_registration(&self) -> &SqlRegistration {
        &self.registration
    }

    pub fn connection_identity(&self) -> Option<crate::connection::ConnectionIdentity> {
        self.connection
    }

    /// Bind the frozen decision to the backend its SQL will run on.
    ///
    /// Binding consumes the captured route so it cannot be attached twice.
    pub fn bind(self, backend: BackendHandle) -> Result<TxRoute, crate::error::DbError> {
        if self.registration.identity() != backend.sql_registration().identity() {
            return Err(crate::error::DbError::config(
                "backend_sql_mismatch",
                "captured SQL registration does not match the resolved backend",
            ));
        }
        if self.connection.is_some() && self.connection != backend.connection_identity() {
            return Err(crate::error::DbError::config(
                "backend_connection_mismatch",
                "captured connection does not match the resolved backend",
            ));
        }
        Ok(TxRoute {
            app_id: self.app_id,
            schema: self.schema,
            in_tx: self.in_tx,
            scope: self.scope,
            backend,
            registration: self.registration,
            connection: self.connection,
        })
    }

    /// Test-only autocommit route with the corresponding built-in registration.
    #[cfg(test)]
    #[doc(hidden)]
    pub fn pool_for_tests(app_id: &str, registration: SqlRegistration) -> Self {
        Self {
            app_id: app_id.to_string(),
            schema: SchemaName::new(app_id).expect("test app ids are legal schema names"),
            in_tx: false,
            scope: None,
            registration,
            connection: None,
        }
    }

    /// Test-only route claiming the app’s currently installed transaction scope.
    #[cfg(test)]
    #[doc(hidden)]
    pub fn tx_for_tests(app_id: &str, registration: SqlRegistration) -> Self {
        Self {
            app_id: app_id.to_string(),
            schema: SchemaName::new(app_id).expect("test app ids are legal schema names"),
            in_tx: true,
            scope: TransactionScope::current(app_id).ok(),
            registration,
            connection: None,
        }
    }
}

impl TxRoute {
    /// Validate the captured callback before admitting work to its lane.
    pub(crate) fn check_scope(&self) -> Result<(), crate::error::DbError> {
        self.scope.as_ref().map_or(Ok(()), TransactionScope::check)
    }

    /// The TENANT this dispatch runs for: the transaction-lane key, the SQLite
    /// ATTACH alias, the metering subject, the CDC stamp.
    ///
    /// NOT the schema. Use [`Self::schema`] to qualify a table or to derive the
    /// PostgreSQL runtime role.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// The PHYSICAL SCHEMA this dispatch qualifies its tables with, and the one
    /// the per-app PostgreSQL role is derived from.
    pub fn schema(&self) -> &SchemaName {
        &self.schema
    }

    /// The backend this dispatch's SQL runs on.
    ///
    /// This is the handle frozen by [`CapturedRoute::bind`].
    pub fn backend(&self) -> &BackendHandle {
        &self.backend
    }

    pub fn sql_registration(&self) -> &SqlRegistration {
        &self.registration
    }

    pub fn connection_identity(&self) -> Option<crate::connection::ConnectionIdentity> {
        self.connection
    }

    /// Whether this call belongs to a transaction callback. An expired scope or busy
    /// transaction session must fail instead of falling back to autocommit.
    /// SQLite autocommit writes can still contend with an open transaction’s writer lock.
    pub fn in_tx(&self) -> bool {
        self.in_tx
    }

    /// Promote this already-captured dispatch onto an internal transaction.
    ///
    /// This is deliberately a consuming conversion rather than another
    /// constructor: the app identity and the original async-scope decision
    /// still have to come from [`CapturedRoute::capture`]. Bulk write fan-out uses it
    /// only after opening either a top-level transaction or a savepoint, so
    /// every statement and its deferred broker event share that frame.
    pub fn into_internal_transaction(mut self) -> Result<Self, crate::error::DbError> {
        self.in_tx = true;
        self.scope = Some(TransactionScope::current(&self.app_id)?);
        Ok(self)
    }
}
