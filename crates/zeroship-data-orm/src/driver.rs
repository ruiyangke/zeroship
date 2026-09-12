//! Database execution contracts. Futures stay local to the compio executor.
//!
//! Drivers bind native values and own physical session cleanup. The ORM owns
//! request authority, operation routing, protection, and transaction policy.
//! Implementations must pass the database conformance suite before registration.

use crate::error::{CleanupAck, DbError, SettleIntent, TerminalResult};
use async_trait::async_trait;
use std::{any::Any, fmt::Debug, ops::Deref, rc::Rc};
use crate::value::Value;
use crate::sql::{compile::SqlDialect};

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
    async fn exec(&self, sql: &str, params: &[Value]) -> Result<u64, DbError>;
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
    pub(crate) fn get_mut<T: 'static>(&mut self) -> Option<&mut T> {
        (self.0.as_mut() as &mut dyn Any).downcast_mut()
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

/// The lifetime of exclusive access required by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseKind {
    /// Commands execute independently of any reserved transaction.
    Autocommit,
    /// Commands share a reserved connection until settlement or drop.
    Transaction,
}

/// A configured physical connection source. It does not select tenants,
/// install authority, begin transactions, or implement application features.
#[async_trait(?Send)]
pub trait Driver: Any + Debug {
    fn dialect(&self) -> SqlDialect;
    fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        None
    }
    /// Return an owned handle with no open transaction. The host selects the
    /// source before acquisition and applies its authority on this same lease.
    async fn acquire(&self, kind: LeaseKind) -> Result<Session, DbError>;
}

#[cfg(test)]
pub(crate) mod tests;
