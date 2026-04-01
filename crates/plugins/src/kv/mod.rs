//! In-memory key-value store plugin for the appbase runtime.
//!
//! Provides a simple, per-isolate KV store that persists for the lifetime
//! of the isolate. Data is lost when the isolate is evicted.
//!
//! JS API:
//!   kv.get(key)        -> string | null
//!   kv.set(key, value) -> void
//!   kv.delete(key)     -> boolean (true if existed)
//!   kv.has(key)        -> boolean
//!   kv.list(prefix)    -> string[] (matching keys)

use appbase_core::plugin::{Aggregation, MeterResource, Plugin, PluginContext};

/// In-memory key-value store plugin.
///
/// Each isolate gets its own KV store. Data does not persist across
/// isolate restarts. For persistent storage, use the `db` plugin.
pub struct KvPlugin;

impl KvPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for KvPlugin {
    fn name(&self) -> &str {
        "kv"
    }

    fn js_bridge(&self) -> &str {
        r#"
globalThis.kv = {
  get: (key) => { throw new Error("kv plugin not yet implemented for raw V8"); },
  set: (key, value) => { throw new Error("kv plugin not yet implemented for raw V8"); },
  delete: (key) => { throw new Error("kv plugin not yet implemented for raw V8"); },
  list: (prefix) => { throw new Error("kv plugin not yet implemented for raw V8"); },
  has: (key) => { throw new Error("kv plugin not yet implemented for raw V8"); },
};
"#
    }

    fn init(&self, _ctx: &mut PluginContext<'_>) {
        // TODO: re-implement with raw V8 ops
    }

    fn meter_resources(&self) -> Vec<MeterResource> {
        vec![
            MeterResource {
                name: "kv.reads".into(),
                unit: "ops".into(),
                aggregation: Aggregation::Sum,
                category: "database".into(),
            },
            MeterResource {
                name: "kv.writes".into(),
                unit: "ops".into(),
                aggregation: Aggregation::Sum,
                category: "database".into(),
            },
        ]
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KvError {
    #[error("Quota exceeded: {0}")]
    QuotaExceeded(#[from] appbase_core::plugin::QuotaDenied),
}
