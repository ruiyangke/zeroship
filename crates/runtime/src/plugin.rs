//! Plugin system — extensible native functions on the `zeroship.*` global.
//!
//! The runtime is a kernel. Plugins are drivers. Each plugin registers
//! functions on `zeroship.{namespace}.*` via the `NativePlugin` trait.
//!
//! The V8 scope IS the context — callbacks read app_id and meter from
//! RuntimeState (via scope slot), and per-thread resources from thread_local.

use std::collections::HashSet;
use std::sync::Arc;

/// A native extension that registers functions on `zeroship.{namespace}.*`.
///
/// Plugins are the extension mechanism for the runtime. Each plugin:
/// 1. Declares a namespace ("db", "auth", "storage", "kv")
/// 2. Registers V8 callbacks in `register()` and poisons any thread-local
///    state it owns (e.g. connection URLs) from `&self` there.
///
/// Callbacks access:
/// - `app_id` → from `scope.get_slot::<SharedState>()` → `state.app_id`
/// - `meter`  → from `scope.get_slot::<SharedState>()` → `state.meter`
/// - resources → from `thread_local!` (pools, caches — initialized lazily
///   on first callback via async bootstrap; see `plugin-db` for the pattern)
///
/// `Send + Sync + 'static` are needed so plugins can live inside an
/// `Arc<dyn NativePlugin>` that crosses worker-thread boundaries in the
/// multi-worker `start_server` path. Plugin authors virtually always
/// satisfy these naturally (URLs, simple structs) — the bound is explicit
/// here so the compiler catches the rare plugin that can't.
pub trait NativePlugin: Send + Sync + 'static {
    /// Namespace under `zeroship.*`. Must be a valid JS identifier.
    /// Examples: "db", "auth", "storage", "kv"
    fn namespace(&self) -> &str;

    /// Human-readable name for logging/debugging.
    fn name(&self) -> &str {
        self.namespace()
    }

    /// Called once per Runtime, on the thread that will own it.
    /// Plugins register V8 callbacks here. Plugins that keep thread-local
    /// resources may initialize them here — the same thread may see
    /// `register()` fire repeatedly as multiple Runtimes are constructed
    /// on it (one per app in multi-tenant workers). Init must be idempotent.
    fn register(&self, r: &mut NativeRegistrar);
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
/// Creates `globalThis.zeroship = { db: { ... }, ... }` with plugin namespaces.
/// Does NOT freeze — call `freeze_zeroship()` after adding any built-in
/// namespaces (e.g. `auth`).
pub(crate) fn register_plugins(scope: &mut v8::PinScope, plugins: &[Arc<dyn NativePlugin>]) {
    let global = scope.get_current_context().global(scope);

    // Always create the zeroship namespace (built-ins like auth need it even
    // when no plugins are registered).
    let zeroship = v8::Object::new(scope);

    // Track registered namespaces so a second plugin claiming the same slot
    // can't silently overwrite the first (callbacks would vanish at runtime
    // with no error). Platform misconfiguration should fail loud.
    let mut seen: HashSet<String> = HashSet::new();

    for plugin in plugins {
        let ns_name = plugin.namespace();
        debug_assert!(
            ns_name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "plugin namespace must be lowercase alphanumeric: {ns_name}"
        );

        if !seen.insert(ns_name.to_string()) {
            panic!(
                "plugin namespace collision: '{}' registered by two plugins (second was '{}')",
                ns_name,
                plugin.name()
            );
        }

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
    }

    // Set zeroship on global
    let zeroship_key = v8::String::new(scope, "zeroship").unwrap();
    global.set(scope, zeroship_key.into(), zeroship.into());
}

/// Freeze the `zeroship` global and all its namespace sub-objects.
///
/// Must be called after `register_plugins()` and any built-in namespace
/// additions (e.g. `auth`). Freezing prevents user code from modifying or
/// monkey-patching platform primitives.
pub(crate) fn freeze_zeroship(scope: &mut v8::PinScope) {
    // Use JS to enumerate and freeze all sub-namespaces, then the root.
    let freeze_js = r#"(function() {
        var zs = globalThis.zeroship;
        if (!zs) return;
        Object.keys(zs).forEach(function(k) {
            if (typeof zs[k] === 'object' && zs[k] !== null) Object.freeze(zs[k]);
        });
        Object.freeze(zs);
    })()"#;
    let code = v8::String::new(scope, freeze_js).unwrap();
    if let Some(script) = v8::Script::compile(scope, code, None) {
        script.run(scope);
    }
}
