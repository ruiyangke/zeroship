//! Capture lifecycle and delivery contracts shared by database adapters.

use super::ChangeEvent;
use crate::binding::DbRoute;

/// Delivery decision stamped when a captured change commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryDisposition {
    Deliver,
    Suppressed,
    SchemaPending,
}

/// Delivery boundary between database capture and the ORM broker.
///
/// **The two methods run on different threads, which is why the trait is
/// `Send + Sync`.** [`Self::disposition`] is sampled by the `commit_hook` on the
/// SQLite writer thread, so the window a suppression guard covers is the set of
/// commits made inside its scope. [`Self::publish`] runs on the compio
/// publisher task, after the packet has crossed the channel. See the
/// "Delivery-window semantics" section of [`crate::backend::sqlite::cdc`] for why the sample
/// point is the commit and not the drain.
pub trait ChangeSink: Send + Sync + 'static {
    /// Whether changes on `route` should be delivered right now.
    ///
    /// The route rather than the tenant, because delivery ownership is per
    /// database: a relay consuming one of an app's databases makes local emit
    /// redundant for THAT database and authoritative for no other.
    ///
    /// Called from inside SQLite's `commit_hook`. Implementations must not
    /// re-enter the connection and must not block for long: this runs on the
    /// single writer thread, in the commit path of every transaction.
    fn disposition(&self, route: &DbRoute) -> DeliveryDisposition;

    /// Hand one decoded change to the consumer. Compio thread.
    fn publish(&self, event: &ChangeEvent);
}
