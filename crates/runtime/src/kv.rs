//! In-memory key-value store for V8 apps.
//! Each isolate gets its own namespace. Data persists across requests
//! but is lost when the isolate is evicted.

use appbase_runtime_macros::appbase_op;

use crate::event_loop::SharedState;

#[appbase_op(state)]
fn kv_get(state: SharedState, key: String) -> Option<String> {
    state.borrow().kv_store.get(&key).cloned()
}

#[appbase_op(state)]
fn kv_set(state: SharedState, key: String, value: String) -> bool {
    state.borrow_mut().kv_store.insert(key, value);
    true
}

#[appbase_op(state)]
fn kv_delete(state: SharedState, key: String) -> bool {
    state.borrow_mut().kv_store.remove(&key).is_some()
}

#[appbase_op(state)]
fn kv_list(state: SharedState) -> Vec<String> {
    state.borrow().kv_store.keys().cloned().collect()
}
