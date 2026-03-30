//! Plugin interface for the appbase runtime.
//!
//! Plugins extend the runtime with new capabilities (db, kv, auth, etc.)
//! by providing:
//! - Rust ops (native functions callable from JS)
//! - JS bridge code (globals injected into the V8 scope)
//! - State initialization (per-isolate state in OpState)
//!
//! # Example
//!
//! ```rust,ignore
//! use appbase_runtime::plugin::Plugin;
//!
//! struct MyPlugin;
//!
//! impl Plugin for MyPlugin {
//!     fn name(&self) -> &str { "my_plugin" }
//!     fn ops(&self) -> Vec<OpDecl> { vec![] }
//!     fn js_bridge(&self) -> &str { "globalThis.myPlugin = {};" }
//!     fn init_state(&self, _state: &mut OpState) {}
//! }
//! ```

use deno_core::{OpDecl, OpState};

/// Plugin interface: ops + JS bridge + state initialization.
///
/// Each plugin is self-contained and provides everything needed
/// to add a new capability to the V8 runtime.
///
/// Plugins must be `Send` because they are passed across thread
/// boundaries (the V8 isolate runs on a dedicated thread).
pub trait Plugin: Send {
    /// Unique name for this plugin (e.g., "db", "kv", "auth").
    fn name(&self) -> &str;

    /// V8 ops registered by this plugin.
    /// These become callable from JS via `Deno.core.ops.op_name()`.
    fn ops(&self) -> Vec<OpDecl>;

    /// JavaScript code injected into the global scope.
    /// Typically defines a global object (e.g., `globalThis.db = {...}`)
    /// that wraps the raw ops into a user-friendly API.
    fn js_bridge(&self) -> &str;

    /// Initialize per-isolate state in the V8 OpState.
    /// Called once when the isolate is created, before any user code runs.
    /// Use this to inject database connections, stores, config, etc.
    fn init_state(&self, state: &mut OpState);
}
