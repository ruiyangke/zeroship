//! Database execution contracts. Futures stay local to the compio executor.
//!
//! Drivers bind native values and own physical session cleanup. The ORM owns
//! request authority, operation routing, protection, and transaction policy.
//! Implementations must pass the database conformance suite before registration.

use crate::{
    binding::DbBinding,
    encryption::KeyStore,
    error::{BeginIntent, CleanupAck, DbError, OpenSessionError, SettleIntent, TerminalResult},
};
use async_trait::async_trait;
use std::{any::Any, fmt::Debug, ops::Deref, rc::Rc};
use zeroship_data_sql::{
    SchemaName,
    catalog::LiveSchema,
    compile::SqlDialect,
    descriptors::{GeoPoint, VectorMetric},
    value::Value,
};

/// Cancellation can either deliver an interrupt or also finish rollback.
#[derive(Debug)]
pub enum CancelDelivery {
    Requested,
    Settled(CleanupAck),
}

#[async_trait(?Send)]
pub trait Cancellation: Debug {
    /// Wait for acknowledgement. A stale token must never interrupt a new lease.
    async fn cancel(&self) -> Result<CancelDelivery, DbError>;
}

/// Independently retained cancellation authority for a particular session lease.
#[derive(Clone, Debug)]
pub struct CancellationHandle(Rc<dyn Cancellation>);
impl CancellationHandle {
    pub fn new(cancellation: impl Cancellation + 'static) -> Self {
        Self(Rc::new(cancellation))
    }
    pub async fn cancel(&self) -> Result<CancelDelivery, DbError> {
        self.0.cancel().await
    }
}

/// An owned lease. Drop must recover or quarantine unfinished work before reuse.
#[async_trait(?Send)]
pub trait DriverSession: Any + Debug {
    fn server_process_id(&self) -> Option<i32> {
        None
    }
    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Value>, DbError>;
    async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError>;
    async fn settle(&self, intent: SettleIntent) -> (TerminalResult, Option<DbError>);
    async fn cleanup(&self) -> CleanupAck;
    fn canceller(&self) -> Option<CancellationHandle>;
    /// Consume a lease that may not be returned to circulation.
    fn discard(self: Box<Self>);
}

/// Backend-independent owned session retained by the transaction lane.
#[derive(Debug)]
pub struct Session(Box<dyn DriverSession>, Option<Rc<()>>);
impl Session {
    pub fn new(session: impl DriverSession) -> Self {
        Self(Box::new(session), None)
    }
    pub(crate) fn bind_driver(mut self, identity: Rc<()>) -> Self {
        self.1 = Some(identity);
        self
    }
    pub(crate) fn validate_driver(&self, identity: &Rc<()>) -> Result<(), DbError> {
        if self
            .1
            .as_ref()
            .is_some_and(|bound| !Rc::ptr_eq(bound, identity))
        {
            return Err(DbError::validation(
                "transaction_backend_changed",
                "this transaction belongs to a different backend registration",
            ));
        }
        Ok(())
    }
    pub fn discard(self) {
        self.0.discard();
    }
    /// Concrete access for backend extensions and driver diagnostics.
    pub fn get<T: 'static>(&self) -> Option<&T> {
        (self.0.as_ref() as &dyn Any).downcast_ref()
    }
}
impl Deref for Session {
    type Target = dyn DriverSession;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

/// Connection acquisition and execution, independent of application models.
#[async_trait(?Send)]
pub trait Driver: Any + Debug {
    fn dialect(&self) -> SqlDialect;
    /// Explicitly declare the source of committed change events.
    fn publishes_committed_changes(&self) -> bool;
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
    /// Return only after BEGIN and session authority setup have succeeded.
    async fn open_tx_session(
        &self,
        app_id: &str,
        schema: &SchemaName,
        begin: BeginIntent,
    ) -> Result<Session, OpenSessionError>;
}

/// Catalog evidence supplies a protection floor; descriptors own model shape.
#[async_trait(?Send)]
pub trait Catalog {
    async fn introspect_schema(&self, app_id: &str) -> Result<LiveSchema, DbError>;
}

/// Optional persistence for host-installed mask policy, separate from execution.
#[async_trait(?Send)]
pub trait PolicyStore {
    async fn persist_mask_policy(&self, app_id: &str, policy: &Value) -> Result<(), DbError>;
    async fn load_mask_policy(&self, app_id: &str) -> Result<Option<Value>, DbError>;
}

#[derive(Debug)]
pub struct VectorSearch<'a> {
    pub binding: &'a DbBinding,
    pub collection: &'a str,
    pub column: &'a str,
    pub query: &'a [f32],
    pub k: usize,
    pub metric: VectorMetric,
    pub filter: &'a Value,
    pub schema: &'a Value,
}
#[derive(Debug)]
pub struct SpatialSearch<'a> {
    pub binding: &'a DbBinding,
    pub collection: &'a str,
    pub column: &'a str,
    pub point: GeoPoint,
    pub radius_m: f64,
    pub filter: &'a Value,
    pub limit: Option<usize>,
    pub schema: &'a Value,
}

/// Database-specific search extensions must use the supplied transaction session.
#[async_trait(?Send)]
pub trait Search {
    async fn vector_search(
        &self,
        session: Option<&Session>,
        request: VectorSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        let _ = (session, request);
        Err(DbError::config(
            "backend_unsupported",
            "the driver does not support vector search",
        ))
    }
    async fn spatial_near(
        &self,
        session: Option<&Session>,
        request: SpatialSearch<'_>,
    ) -> Result<Vec<Value>, DbError> {
        let _ = (session, request);
        Err(DbError::config(
            "backend_unsupported",
            "the driver does not support spatial search",
        ))
    }
}

/// Registration unit combining execution with the ORM's required host services.
/// This interface carries no migration, backup, or privileged CDC capability.
pub trait Backend: Driver + Catalog + PolicyStore + Search {
    fn key_store(&self) -> &KeyStore;
}
