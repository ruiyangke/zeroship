//! Consumer-owned port for SQLite CDC delivery.

use zeroship_core::change_event::ChangeEvent;

/// SQLite publisher action sampled for one dequeued change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryDisposition {
    Deliver,
    Suppressed,
    SchemaPending,
}

/// Delivery boundary implemented by the engine that consumes SQLite changes.
pub(crate) trait ChangeSink: 'static {
    fn disposition(&self, app_id: &str) -> DeliveryDisposition;
    fn publish(&self, event: &ChangeEvent);
}
