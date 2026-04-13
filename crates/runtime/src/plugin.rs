//! Plugin system — extensible native functions on the `zeroship.*` global.
//!
//! The runtime is a kernel. Plugins are drivers. Each plugin registers
//! functions on `zeroship.{namespace}.*` via the `NativePlugin` trait.
//!
//! The V8 scope IS the context — callbacks read app_id and meter from
//! RuntimeState (via scope slot), and per-thread resources from thread_local.

use std::sync::Arc;

/// Configuration passed to plugins during init.
/// Contains worker-level config (DB URL, storage path, etc.)
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// Postgres connection URL (for database plugins).
    pub db_url: Option<String>,
    /// JWT secret (for auth plugins).
    pub jwt_secret: Option<String>,
    /// Storage path or S3 URL (for storage plugins).
    pub storage_url: Option<String>,
    /// Arbitrary key-value config for custom plugins.
    pub extra: std::collections::HashMap<String, String>,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            db_url: None,
            jwt_secret: None,
            storage_url: None,
            extra: std::collections::HashMap::new(),
        }
    }
}

/// A native extension that registers functions on `zeroship.{namespace}.*`.
///
/// Plugins are the extension mechanism for the runtime. Each plugin:
/// 1. Declares a namespace ("db", "auth", "storage", "kv")
/// 2. Initializes per-thread resources in `init()` (connection pools, caches)
/// 3. Registers V8 callbacks in `register()` (called per-isolate)
///
/// Callbacks access:
/// - `app_id` → from `scope.get_slot::<SharedState>()` → `state.app_id`
/// - `meter`  → from `scope.get_slot::<SharedState>()` → `state.meter`
/// - resources → from `thread_local!` (pools, caches — set in `init()`)
pub trait NativePlugin: Send + Sync {
    /// Namespace under `zeroship.*`. Must be a valid JS identifier.
    /// Examples: "db", "auth", "storage", "kv"
    fn namespace(&self) -> &str;

    /// Human-readable name for logging/debugging.
    fn name(&self) -> &str {
        self.namespace()
    }

    /// Called once per worker thread. Set up thread-local resources
    /// (connection pools, caches). Can do async I/O.
    fn init(&self, config: &Arc<PluginConfig>);

    /// Called once per V8 isolate. Register functions on `zeroship.{namespace}`.
    fn register(&self, registrar: &mut NativeRegistrar);

    /// Called on worker shutdown. Close connections, flush buffers.
    fn shutdown(&self) {}
}

/// Collects function registrations from a plugin.
///
/// Plugins call `registrar.add("find", my_callback)` which stores a boxed
/// closure that creates the V8 function when given a scope. This avoids
/// storing V8 types (which require a scope) and avoids borrow conflicts.
pub struct NativeRegistrar {
    /// Collected registration closures: each creates one V8 function on the namespace object.
    pub(crate) entries: Vec<(
        &'static str,
        Box<dyn Fn(&mut v8::PinScope, v8::Local<v8::Object>)>,
    )>,
}

impl NativeRegistrar {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Register a native function as `zeroship.{namespace}.{name}`.
    ///
    /// The callback signature:
    /// `fn(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue)`
    pub fn add<F>(&mut self, name: &'static str, callback: F)
    where
        F: v8::MapFnTo<v8::FunctionCallback> + 'static,
    {
        self.entries.push((
            name,
            Box::new(move |scope: &mut v8::PinScope, ns_obj: v8::Local<v8::Object>| {
                let func = v8::Function::new(scope, callback).unwrap();
                let key = v8::String::new(scope, name).unwrap();
                ns_obj.set(scope, key.into(), func.into());
            }),
        ));
    }
}

/// Register all plugins on the `zeroship` global namespace.
///
/// Creates `globalThis.zeroship = { db: { ... }, auth: { ... }, ... }`
/// and freezes the entire object tree to prevent modification by user code.
pub(crate) fn register_plugins(scope: &mut v8::PinScope, plugins: &[Box<dyn NativePlugin>]) {
    if plugins.is_empty() {
        return;
    }

    let global = scope.get_current_context().global(scope);
    let zeroship = v8::Object::new(scope);

    // Collect namespace names for the freeze step
    let mut namespaces: Vec<String> = Vec::new();

    for plugin in plugins {
        let ns_name = plugin.namespace();
        debug_assert!(
            ns_name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "plugin namespace must be lowercase alphanumeric: {ns_name}"
        );

        // Collect registrations from the plugin (no V8 scope needed)
        let mut registrar = NativeRegistrar::new();
        plugin.register(&mut registrar);

        // Create V8 namespace object and apply all registrations
        let ns_obj = v8::Object::new(scope);
        for (_name, apply_fn) in &registrar.entries {
            apply_fn(scope, ns_obj);
        }

        let ns_key = v8::String::new(scope, ns_name).unwrap();
        zeroship.set(scope, ns_key.into(), ns_obj.into());
        namespaces.push(ns_name.to_string());
    }

    // Set zeroship on global
    let zeroship_key = v8::String::new(scope, "zeroship").unwrap();
    global.set(scope, zeroship_key.into(), zeroship.into());

    // Freeze everything in one script (no borrow conflicts)
    let mut freeze_js = String::from("Object.freeze(globalThis.zeroship);");
    for ns in &namespaces {
        freeze_js.push_str(&format!("Object.freeze(globalThis.zeroship.{ns});"));
    }
    let code = v8::String::new(scope, &freeze_js).unwrap();
    if let Some(script) = v8::Script::compile(scope, code, None) {
        script.run(scope);
    }
}
