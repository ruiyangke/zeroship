//! Consumer-owned port for SQLite CDC delivery.

use zeroship_core::change_event::ChangeEvent;

/// SQLite publisher action for one change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryDisposition {
    Deliver,
    Suppressed,
    SchemaPending,
}

/// Delivery boundary implemented by the engine that consumes SQLite changes.
///
/// **The two methods run on different threads, which is why the trait is
/// `Send + Sync`.** [`Self::disposition`] is sampled by the `commit_hook` on the
/// SQLite writer thread, so the window a suppression guard covers is the set of
/// commits made inside its scope. [`Self::publish`] runs on the compio
/// publisher task, after the packet has crossed the channel. See the
/// "Delivery-window semantics" section of [`crate::cdc`] for why the sample
/// point is the commit and not the drain.
pub trait ChangeSink: Send + Sync + 'static {
    /// Whether changes for `app_id` should be delivered right now.
    ///
    /// Called from inside SQLite's `commit_hook`. Implementations must not
    /// re-enter the connection and must not block for long: this runs on the
    /// single writer thread, in the commit path of every transaction.
    fn disposition(&self, app_id: &str) -> DeliveryDisposition;

    /// Hand one decoded change to the consumer. Compio thread.
    fn publish(&self, event: &ChangeEvent);
}
