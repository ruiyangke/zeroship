//! Environment variables plugin (stub for plugin registry).
//!
//! Env var access is now handled natively by the V8 runtime via the
//! `env.get(key)` global, which reads `APPBASE_APP_{KEY}` from the process
//! environment. This plugin exists for backward compatibility only.

use appbase_core::plugin::{Plugin, PluginContext};

/// Environment variables plugin (stub).
///
/// Environment variable access is now provided natively by the V8 runtime
/// via the `env.get(key)` global. This plugin exists only for backward
/// compatibility with the plugin registry; it does not inject any JS bridge
/// of its own.
pub struct EnvPlugin;

impl EnvPlugin {
    /// Create an env plugin. Variables are handled natively by the V8
    /// runtime (`env.get(key)` reads `APPBASE_APP_{KEY}` from process env).
    pub fn new() -> Self {
        Self
    }

    /// Backward-compatible constructor. The prefix is ignored — env access
    /// is now handled by the native V8 `env.get()` global.
    pub fn from_system(_prefix: &str) -> Self {
        Self
    }
}

impl Plugin for EnvPlugin {
    fn name(&self) -> &str {
        "env"
    }

    fn js_bridge(&self) -> &str {
        // env.get() is now provided natively by the V8 globals; no JS bridge needed.
        ""
    }

    fn init(&self, _ctx: &mut PluginContext<'_>) {
        // No-op: env access is handled by the native V8 env.get() global.
    }
}
