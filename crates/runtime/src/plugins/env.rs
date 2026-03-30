//! Environment variables / secrets plugin for the appbase runtime.
//!
//! Provides read-only access to a configured set of environment variables.
//! Does NOT expose the full system environment — only variables explicitly
//! passed at construction time, for security.
//!
//! JS API:
//!   env.get(key) → string | null
//!   env.list()   → string[] (all available keys)

use crate::v8::Plugin;
use deno_core::op2;
use deno_core::{OpDecl, OpState};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Environment variables plugin.
///
/// Variables are set at isolate creation time and are read-only.
/// Use `from_system("APPBASE_")` to capture env vars with a prefix,
/// or `new(vars)` to inject explicit key-value pairs.
pub struct EnvPlugin {
    vars: HashMap<String, String>,
}

/// Newtype wrapper to avoid OpState type collision with KvStore and DbConnection.
pub struct EnvStore(pub RefCell<HashMap<String, String>>);

impl EnvPlugin {
    /// Create with explicit variables.
    pub fn new(vars: HashMap<String, String>) -> Self {
        Self { vars }
    }

    /// Create from system environment, filtered by prefix.
    /// e.g., `from_system("APPBASE_")` captures `APPBASE_API_KEY`, etc.
    /// Only variables starting with the prefix are exposed to user code.
    pub fn from_system(prefix: &str) -> Self {
        let vars: HashMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with(prefix))
            .collect();
        Self { vars }
    }
}

impl Plugin for EnvPlugin {
    fn name(&self) -> &str {
        "env"
    }

    fn ops(&self) -> Vec<OpDecl> {
        vec![op_env_get(), op_env_list()]
    }

    fn js_bridge(&self) -> &str {
        r#"
globalThis.env = {
  get: (key) => Deno.core.ops.op_env_get(key),
  list: () => Deno.core.ops.op_env_list(),
};
"#
    }

    fn init_state(&self, state: &mut OpState) {
        state.put(Rc::new(EnvStore(RefCell::new(self.vars.clone()))));
    }
}

/// Get an environment variable by key. Returns None if not set.
#[op2]
#[string]
pub fn op_env_get(state: &mut OpState, #[string] key: &str) -> Option<String> {
    let store = state.borrow::<Rc<EnvStore>>().clone();
    store.0.borrow().get(key).cloned()
}

/// List all available environment variable keys.
#[op2]
#[serde]
pub fn op_env_list(state: &mut OpState) -> Vec<String> {
    let store = state.borrow::<Rc<EnvStore>>().clone();
    store.0.borrow().keys().cloned().collect()
}
