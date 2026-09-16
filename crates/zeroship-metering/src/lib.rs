//! Metering infrastructure — the platform-measured usage signal.
//!
//! This crate is the home of the per-worker [`Meter`] (atomic
//! per-`(app_id, metric)` counters) and the compio [`outbox`] task that
//! drains it into `UsageEvent` records and publishes them to the durable
//! stream. It carries **no V8** and depends only on `zeroship-core`
//! (wire types), `zeroship-stream`, `compio`, `serde_json`, and `uuid`.
//!
//! ## Metering is infrastructure, not a creator API
//!
//! There is deliberately **no `env.meter` creator namespace**. The
//! billing signal is platform-measured so app code can neither forge nor
//! suppress it:
//!
//! - The worker emits the five fixed platform counters (`requests`,
//!   `cpu_us`, `wall_us`, `egress_bytes`, `ingress_bytes`) via
//!   [`Meter::record_request`] once per dispatched request.
//! - The trusted data primitives (`plugin-db`, `kv-v8`,
//!   `storage-v8`) emit raw usage metrics (`db_reads`, `db_writes`,
//!   `kv_reads`, `kv_writes`, `storage_ops`, …) at their op boundary, in
//!   the success arm only, via a [`MeterHandle`].
//!
//! A [`MeterHandle`] is the injection vehicle: the process-wide
//! `Arc<Meter>` bound to one server-injected `app_id`, so a producer
//! emits without re-deriving the app and cannot meter another app.

use std::sync::Arc;
use zeroship_core::AppId;

pub mod meter;
pub mod outbox;

// No env consumer is declared here, and that is deliberate: the reads happen in
// each binary's own resolver under its own registered name (`metering.brokers`
// and its three siblings are generated settings). A library consumer with nothing
// left to read would be a registry entry claiming reads that do not exist.

pub use meter::Meter;
pub use outbox::{
    build_usage_outbox, spawn_disabled_drain_task, spawn_outbox_task, wal_identity, OutboxConfig,
    OutboxFailure, OutboxPublishResult, UsageOutbox, UsageStreamSettings, WalIdentity,
    DEFAULT_OUTBOX_INTERVAL, DEFAULT_USAGE_EVENTS_TOPIC, OUTBOX_DISABLED_LOG,
};

/// The injection vehicle for the trusted producers (the db/kv/storage
/// native primitives). Binds the process-wide `Arc<Meter>` to the
/// isolate's server-injected `app_id` so a producer emits a metric
/// without re-deriving the app — and structurally cannot meter another
/// app.
///
/// This replaces the deleted `env.meter` v8_class: the same per-app
/// scoping guarantee, but applied to platform primitives (Rust the
/// creator cannot reach) instead of user code.
#[derive(Clone)]
pub struct MeterHandle {
    meter: Arc<Meter>,
    app_id: AppId,
}

impl std::fmt::Debug for MeterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeterHandle").field("app_id", &self.app_id).finish()
    }
}

impl MeterHandle {
    /// Bind the shared `Arc<Meter>` to one `app_id`. The `app_id` is the
    /// server-injected `SharedState.env_vars["APP_ID"]` the runtime hands
    /// each plugin's `build_instance(scope, app_id)` — never a value JS
    /// supplies.
    #[must_use]
    pub fn new(meter: Arc<Meter>, app_id: AppId) -> Self {
        Self { meter, app_id }
    }

    /// Record successful work against the bound app without asynchronous I/O.
    pub fn record(&self, metric: &str, n: u64) {
        self.meter.increment(&self.app_id, metric, n);
    }

    /// The app this handle meters (for tests / diagnostics).
    #[must_use]
    pub fn app_id(&self) -> &AppId {
        &self.app_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_preserves_the_typed_app_identity_through_drain() {
        let app = zeroship_core::AppId::mint();
        let meter = Arc::new(Meter::new());
        let handle = MeterHandle::new(Arc::clone(&meter), app.clone());
        let bound: &zeroship_core::AppId = handle.app_id();
        assert_eq!(bound, &app);
        meter.increment(&app, "requests", 1);
        handle.record("db_reads", 2);
        let events = meter.drain();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| event.subject.app.as_ref() == Some(&app)));
    }

    #[test]
    fn handle_records_scoped_to_its_app() {
        let meter = Arc::new(Meter::new());
        let a = AppId::mint();
        let b = AppId::mint();

        let ha = MeterHandle::new(Arc::clone(&meter), a.clone());
        let hb = MeterHandle::new(Arc::clone(&meter), b.clone());

        ha.record("db_writes", 2);
        ha.record("db_writes", 1);
        hb.record("db_writes", 5);

        let events = meter.drain();
        assert_eq!(event_value(&events, &a, "db_writes"), Some(3));
        assert_eq!(event_value(&events, &b, "db_writes"), Some(5));
    }

    fn event_value(
        events: &[zeroship_core::usage_event::UsageEvent],
        app_id: &zeroship_core::app_id::AppId,
        meter: &str,
    ) -> Option<u64> {
        events
            .iter()
            .find(|event| event.subject.app.as_ref() == Some(app_id) && event.meter == meter)
            .map(|event| event.value)
    }
}
