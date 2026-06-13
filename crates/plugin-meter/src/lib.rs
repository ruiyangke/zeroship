//! Metering plugin — the `env.meter.*` native primitive.
//!
//! `env.meter` is a `#[v8_class]` instance (`MeterHandle`) minted once per
//! isolate by [`MeterPlugin::build_instance`]. The instance carries the
//! shared per-worker [`Meter`] handle and the isolate's `app_id`, so
//! `env.meter.increment(metric, n?)` bumps the right app's counters with no
//! per-call app lookup and no way for user code to meter another app.
//!
//! Native surface (wrapped by SDK packages — creators don't call this
//! directly):
//! - `env.meter.increment(metric: string, n?: number)` → number
//!   (the metric's new running total this billing period)
//!
//! The five fixed platform counters (requests, cpu_us, wall_us,
//! egress_bytes, ingress_bytes) and SDK-defined `custom` metrics share one
//! [`Meter`]. A single [`flush`] task per worker process drains it every
//! ~10s and POSTs a `UsageReport` (with a monotonic per-worker sequence) to
//! control for idempotent aggregation.
//!
//! Mirrors `plugin-kv`'s structure (`lib` → `MeterPlugin`; `v8_class` →
//! `MeterHandle`/`mint_meter`), but with NO async dispatch module — an
//! increment is a synchronous atomic bump, not a backend round trip.

use std::sync::Arc;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

pub mod flush;
pub mod meter;
pub mod v8_class;

pub use flush::{spawn_flush_task, FlushConfig, DEFAULT_FLUSH_INTERVAL};
pub use meter::{build_report, is_fixed_metric, Meter, SequenceSource, FIXED_METRICS};
pub use v8_class::{mint_meter, MeterHandle};

/// The `env.meter` plugin. Holds a shared `Arc<Meter>` — the SAME instance
/// the flush task drains — so increments from any isolate on any worker
/// thread accumulate into one place.
pub struct MeterPlugin {
    meter: Arc<Meter>,
}

impl std::fmt::Debug for MeterPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeterPlugin").finish()
    }
}

impl MeterPlugin {
    /// Construct over a shared meter. The caller owns the `Arc<Meter>` and
    /// passes the same one to [`spawn_flush_task`], so the producer
    /// (isolates) and the drainer (flush task) share state.
    #[must_use]
    pub fn with_meter(meter: Arc<Meter>) -> Self {
        Self { meter }
    }
}

impl NativePlugin for MeterPlugin {
    fn namespace(&self) -> &str {
        "meter"
    }
    fn name(&self) -> &str {
        "meter"
    }

    /// No flat callbacks — the whole surface lives on the `MeterHandle`
    /// v8_class minted by [`Self::build_instance`].
    fn register(&self, _r: &mut NativeRegistrar) {}

    fn build_instance<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        mint_meter(scope, Arc::clone(&self.meter), app_id)
    }
}
