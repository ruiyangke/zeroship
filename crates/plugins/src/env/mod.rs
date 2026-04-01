//! Environment variables / secrets plugin for the appbase runtime.
//!
//! Provides read-only access to a configured set of environment variables.
//! Does NOT expose the full system environment — only variables explicitly
//! passed at construction time, for security.
//!
//! JS API:
//!   env.get(key) -> string | null
//!   env.list()   -> string[] (all available keys)

use appbase_core::plugin::{Plugin, PluginContext};
use std::collections::HashMap;

/// Environment variables plugin.
///
/// Variables are set at isolate creation time and are read-only.
/// Use `from_system("APPBASE_")` to capture env vars with a prefix,
/// or `new(vars)` to inject explicit key-value pairs.
pub struct EnvPlugin {
    vars: HashMap<String, String>,
}

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

    fn js_bridge(&self) -> &str {
        r#"
globalThis.env = {
  get: (key) => { throw new Error("env plugin not yet implemented for raw V8"); },
  list: () => { throw new Error("env plugin not yet implemented for raw V8"); },
};
"#
    }

    fn init(&self, _ctx: &mut PluginContext<'_>) {
        // TODO: re-implement with raw V8 ops
        // Will store self.vars in the V8 isolate context
    }
}
