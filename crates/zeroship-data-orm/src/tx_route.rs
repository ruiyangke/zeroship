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
//! the physical schema and immutable SQL registration used by query preparation,
//! and the usage sink its host attached to the binding.

use std::sync::Arc;

use crate::backend::BackendHandle;
use crate::binding::DbBinding;
use crate::metrics::UsageSink;
use crate::sql::SchemaName;
use crate::sql::registration::SqlRegistration;
use crate::transaction::scope::TransactionScope;

/// A synchronous routing decision awaiting backend binding.
/// Capture callback identity before asynchronous connection setup can yield.
#[derive(Debug)]
pub struct CapturedRoute {
    app_id: String,
    /// Physical schema used for SQL qualification and PostgreSQL role selection.
    /// App identity remains the transaction-lane key.
    schema: SchemaName,
    /// `true` iff this dispatch is lexically-and-asynchronously inside a
    /// `db.transaction(fn)` callback **for this same app**.
    in_tx: bool,
    scope: Option<TransactionScope>,
    /// Compiler, codecs, and effective support captured before backend acquisition.
    registration: SqlRegistration,
    connection: CapturedConnection,
    /// Where this dispatch reports its usage; `None` reports nothing.
    usage: Option<Arc<dyn UsageSink>>,
}

#[derive(Debug)]
enum CapturedConnection {
    Bound(crate::connection::ConnectionIdentity),
    #[cfg(test)]
    Unbound,
}

/// A captured dispatch bound to its backend. It carries app identity, callback
/// scope and SQL registration together, so execution does not re-read host context.
/// Construct through [`CapturedRoute::bind`].
#[derive(Clone, Debug)]
pub struct TxRoute {
    app_id: String,
    usage: Option<Arc<dyn UsageSink>>,
    schema: SchemaName,
    in_tx: bool,
    scope: Option<TransactionScope>,
    backend: BackendHandle,
    registration: SqlRegistration,
    connection: crate::connection::ConnectionIdentity,
}

fn validate_binding_target(
    app_id: &str,
    schema: &SchemaName,
    binding: &DbBinding,
) -> Result<(), crate::error::DbError> {
    if app_id != binding.app_id() || schema != binding.schema() {
        return Err(crate::error::DbError::internal(
            "ORM binding does not match the captured database route",
        ));
    }
    Ok(())
}

impl CapturedRoute {
    /// Freeze the host's observed async scope for this dispatch. A scope for
    /// another app does not confer access to this app's transaction.
    ///
    /// `usage` is the sink the host attached to this binding. Every capture
    /// site states it, so a host cannot meter one entry point and forget
    /// another; `None` reports nothing and never refuses the dispatch.
    pub fn capture(
        current_scope: Option<&TransactionScope>,
        app_id: &str,
        schema: SchemaName,
        registration: SqlRegistration,
        connection: crate::connection::ConnectionIdentity,
        usage: Option<Arc<dyn UsageSink>>,
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
            connection: CapturedConnection::Bound(connection),
            usage,
        }
    }

    /// The physical schema this dispatch qualifies its tables with.
    pub fn schema(&self) -> &SchemaName {
        &self.schema
    }

    /// App identity used for transaction lanes.
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

    pub(crate) fn validate_binding(
        &self,
        binding: &DbBinding,
    ) -> Result<(), crate::error::DbError> {
        validate_binding_target(&self.app_id, &self.schema, binding)
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
        let connection = match self.connection {
            CapturedConnection::Bound(connection) => {
                if connection != backend.connection_identity() {
                    return Err(crate::error::DbError::config(
                        "backend_connection_mismatch",
                        "captured connection does not match the resolved backend",
                    ));
                }
                connection
            }
            #[cfg(test)]
            CapturedConnection::Unbound => backend.connection_identity(),
        };
        Ok(TxRoute {
            usage: self.usage,
            app_id: self.app_id,
            schema: self.schema,
            in_tx: self.in_tx,
            scope: self.scope,
            backend,
            registration: self.registration,
            connection,
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
            connection: CapturedConnection::Unbound,
            usage: None,
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
            connection: CapturedConnection::Unbound,
            usage: None,
        }
    }

    /// Test-only: report this route's usage to `sink`.
    #[cfg(test)]
    pub(crate) fn with_usage_for_tests(mut self, sink: Arc<dyn UsageSink>) -> Self {
        self.usage = Some(sink);
        self
    }
}

impl TxRoute {
    pub(crate) fn validate_binding(
        &self,
        binding: &DbBinding,
    ) -> Result<(), crate::error::DbError> {
        validate_binding_target(&self.app_id, &self.schema, binding)
    }

    /// The sink this dispatch reports successful work to, if its host attached one.
    pub(crate) fn usage(&self) -> Option<&dyn UsageSink> {
        self.usage.as_deref()
    }

    /// Validate the captured callback before admitting work to its lane.
    pub(crate) fn check_scope(&self) -> Result<(), crate::error::DbError> {
        self.scope.as_ref().map_or(Ok(()), TransactionScope::check)
    }

    /// The TENANT this dispatch runs for: the transaction-lane key, the SQLite
    /// ATTACH alias, the CDC stamp.
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

    pub fn connection_identity(&self) -> crate::connection::ConnectionIdentity {
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
