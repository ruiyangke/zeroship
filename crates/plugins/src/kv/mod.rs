//! In-memory key-value store plugin for the appbase runtime.
//!
//! Provides a simple, per-isolate KV store that persists for the lifetime
//! of the isolate. Data is lost when the isolate is evicted.
//!
//! JS API:
//!   kv.get(key)        → string | null
//!   kv.set(key, value) → void
//!   kv.delete(key)     → boolean (true if existed)
//!   kv.has(key)        → boolean
//!   kv.list(prefix)    → string[] (matching keys)

use appbase_core::plugin::{Aggregation, MeterResource, Plugin, PluginContext, PluginMeter};
use deno_core::op2;
use deno_core::{OpDecl, OpState};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

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

/// Newtype wrapper around HashMap to avoid OpState type collision
/// with other plugins that also store HashMap<String, String>.
/// OpState uses TypeId for lookup, so each plugin needs a unique type.
pub struct KvStore(pub RefCell<HashMap<String, String>>);

impl Plugin for KvPlugin {
    fn name(&self) -> &str {
        "kv"
    }

    fn ops(&self) -> Vec<OpDecl> {
        vec![
            op_kv_get(),
            op_kv_set(),
            op_kv_delete(),
            op_kv_list(),
            op_kv_has(),
        ]
    }

    fn js_bridge(&self) -> &str {
        r#"
globalThis.kv = {
  get: (key) => Deno.core.ops.op_kv_get(key),
  set: (key, value) => Deno.core.ops.op_kv_set(key, typeof value === 'string' ? value : JSON.stringify(value)),
  delete: (key) => Deno.core.ops.op_kv_delete(key),
  list: (prefix) => Deno.core.ops.op_kv_list(prefix || ''),
  has: (key) => Deno.core.ops.op_kv_has(key),
};
"#
    }

    fn init(&self, ctx: &mut PluginContext<'_>) {
        ctx.op_state.put(Rc::new(KvStore(RefCell::new(HashMap::new()))));
        ctx.op_state.put(KvMeter(ctx.meter.clone()));
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

/// Newtype wrapper around the meter to avoid OpState type collision.
struct KvMeter(Arc<dyn PluginMeter>);

/// Get a value by key. Returns None if key doesn't exist.
#[op2]
#[string]
pub fn op_kv_get(state: &mut OpState, #[string] key: &str) -> Option<String> {
    let store = state.borrow::<Rc<KvStore>>().clone();
    state.borrow::<KvMeter>().0.increment("kv.reads", 1);
    store.0.borrow().get(key).cloned()
}

/// Set a key-value pair. Overwrites existing value.
#[op2(fast)]
pub fn op_kv_set(state: &mut OpState, #[string] key: &str, #[string] value: &str) {
    let store = state.borrow::<Rc<KvStore>>().clone();
    state.borrow::<KvMeter>().0.increment("kv.writes", 1);
    store
        .0
        .borrow_mut()
        .insert(key.to_string(), value.to_string());
}

/// Delete a key. Returns true if the key existed.
#[op2(fast)]
pub fn op_kv_delete(state: &mut OpState, #[string] key: &str) -> bool {
    let store = state.borrow::<Rc<KvStore>>().clone();
    state.borrow::<KvMeter>().0.increment("kv.writes", 1);
    store.0.borrow_mut().remove(key).is_some()
}

/// List all keys matching a prefix. Empty prefix returns all keys.
#[op2]
#[serde]
pub fn op_kv_list(state: &mut OpState, #[string] prefix: &str) -> Vec<String> {
    let store = state.borrow::<Rc<KvStore>>().clone();
    state.borrow::<KvMeter>().0.increment("kv.reads", 1);
    store
        .0
        .borrow()
        .keys()
        .filter(|k| k.starts_with(prefix))
        .cloned()
        .collect()
}

/// Check if a key exists.
#[op2(fast)]
pub fn op_kv_has(state: &mut OpState, #[string] key: &str) -> bool {
    let store = state.borrow::<Rc<KvStore>>().clone();
    state.borrow::<KvMeter>().0.increment("kv.reads", 1);
    store.0.borrow().contains_key(key)
}
