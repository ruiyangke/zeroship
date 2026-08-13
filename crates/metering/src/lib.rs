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
//! - The trusted data primitives (`plugin-db`, `plugin-kv`,
//!   `plugin-storage`) emit raw usage metrics (`db_reads`, `db_writes`,
//!   `kv_reads`, `kv_writes`, `storage_ops`, …) at their op boundary, in
//!   the success arm only, via a [`MeterHandle`].
//!
//! A [`MeterHandle`] is the injection vehicle: the process-wide
//! `Arc<Meter>` bound to one server-injected `app_id`, so a producer
//! emits without re-deriving the app and cannot meter another app.

use std::sync::Arc;

pub mod meter;
pub mod outbox;

zeroship_core::declare_env_consumer!(
    /// The metering crate's own environment reads.
    ///
    /// A LIBRARY consumer, so `target` is the cargo package rather than a
    /// binary: `UsageStreamSettings::from_env` is called by both
    /// `zeroship-worker` and `zeroship-gate`, and naming either one would put
    /// a fiction in the record.
    pub MeteringConsumer,
    target = "zeroship-metering",
    scope = "metering");

pub use meter::Meter;
pub use outbox::{
    build_usage_outbox, spawn_disabled_drain_task, spawn_outbox_task, wal_identity, OutboxConfig,
    OutboxFailure, OutboxPublishResult, UsageOutbox, UsageStreamSettings, WalIdentity,
    DEFAULT_OUTBOX_INTERVAL, DEFAULT_USAGE_EVENTS_TOPIC,
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
    app_id: String,
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
    pub fn new(meter: Arc<Meter>, app_id: impl Into<String>) -> Self {
        Self { meter, app_id: app_id.into() }
    }

    /// Emit `n` of `metric` against this handle's app. A synchronous,
    /// lock-free atomic bump — adds no await and cannot fail the op it
    /// rides on, so producers call it in the op's success arm.
    pub fn record(&self, metric: &str, n: u64) {
        self.meter.increment(&self.app_id, metric, n);
    }

    /// The app this handle meters (for tests / diagnostics).
    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn handle_records_scoped_to_its_app() {
        let meter = Arc::new(Meter::new());
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();

        let ha = MeterHandle::new(Arc::clone(&meter), a.clone());
        let hb = MeterHandle::new(Arc::clone(&meter), b.clone());

        ha.record("db_writes", 2);
        ha.record("db_writes", 1);
        hb.record("db_writes", 5);

        let events = meter.drain();
        let ia = Uuid::parse_str(&a).unwrap();
        let ib = Uuid::parse_str(&b).unwrap();
        assert_eq!(event_value(&events, ia, "db_writes"), Some(3));
        assert_eq!(event_value(&events, ib, "db_writes"), Some(5));
    }

    fn event_value(
        events: &[zeroship_core::usage_event::UsageEvent],
        app_id: Uuid,
        meter: &str,
    ) -> Option<u64> {
        events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .map(|event| event.value)
    }
}
