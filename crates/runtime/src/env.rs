//! Per-app environment variable access for V8 apps.
//!
//! Each isolate has its own `env_vars` map injected at creation time.
//! Apps cannot access host process environment or other apps' secrets.

use appbase_runtime_macros::appbase_op;

use crate::state::SharedState;

/// `env.get(key) → string | null`
///
/// Reads from the per-app environment variables injected at deploy time.
#[appbase_op(state)]
fn env_get(state: SharedState, key: String) -> Option<String> {
    state.borrow().env_vars.get(&key).cloned()
}
