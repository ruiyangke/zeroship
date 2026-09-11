//! Capture lifecycle and delivery contracts shared by database adapters.

use super::ChangeEvent;
use crate::error::DbError;

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
    /// Whether changes for `app_id` should be delivered right now.
    ///
    /// Called from inside SQLite's `commit_hook`. Implementations must not
    /// re-enter the connection and must not block for long: this runs on the
    /// single writer thread, in the commit path of every transaction.
    fn disposition(&self, app_id: &str) -> DeliveryDisposition;

    /// Hand one decoded change to the consumer. Compio thread.
    fn publish(&self, event: &ChangeEvent);
}

pub trait ChangeStream: 'static {
    /// Concrete handle representing a spawned-but-still-running
    /// consumer. PG: a task handle / supervisor handle; SQLite: a
    /// session marker the actor uses to track that hooks are armed.
    /// Type erased per-impl (associated type) so we don't pay the
    /// `Box<dyn Future>` price the dyn-safe shape would force.
    type ConsumerHandle: 'static;

    /// Idempotently tear down the CDC infrastructure for `app_id`.
    /// Used during app deletion; PG drops the publication and every worker
    /// slot, while SQLite disarms hooks.
    #[allow(async_fn_in_trait)]
    async fn deprovision(&self, app_id: &str) -> Result<(), DbError>;

    /// Provision and spawn the long-running consumer for `(app_id,
    /// worker_id)`. This is the sole provisioning path so a slot cannot be
    /// created without an owned task. The returned handle controls explicit
    /// shutdown and completion.
    #[allow(async_fn_in_trait)]
    async fn spawn_consumer(
        &self,
        app_id: &str,
        worker_id: &str,
    ) -> Result<Self::ConsumerHandle, DbError>;

    // Pause and schema-pending guards belong to cdc::broker, which owns the
    // delivery registries they change.
}
