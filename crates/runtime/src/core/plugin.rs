//! Plugin system — extensible native functions on the `env.*` handler arg.
//!
//! The runtime is a kernel. Plugins are drivers. Each plugin registers a set
//! of native callbacks under its own namespace (e.g. "db", "kv"). The
//! namespaces are overlaid onto the per-app scalar env JSON to form the
//! single composite `env` object that user code sees as
//!   - the `env` argument of `fetch(request, env, ctx)`
//!   - the `env` named export of the `zeroship` module
//!   - the return value of `__zs_env()`
//!
//! The V8 scope IS the context — callbacks read `app_id` / `meter` from
//! `RuntimeState` via the isolate scope slot, and per-thread resources from
//! `thread_local!`.

use std::collections::HashSet;
use std::sync::Arc;

/// A native extension that registers functions on `env.{namespace}.*`.
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
    /// Namespace under `env.*`. Must be a valid JS identifier — lowercase
    /// alphanumeric + underscore. Examples: "db", "auth", "storage", "kv"
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

    /// Optional hook: build the V8 namespace object yourself instead of
    /// letting the runtime allocate a plain `v8::Object` for you.
    ///
    /// Default returns `None`. When `None`, `build_env_object` allocates
    /// a fresh `v8::Object`, then layers the callbacks from
    /// [`NativePlugin::register`] onto it — the historical behaviour.
    ///
    /// When this returns `Some(obj)`, the runtime uses `obj` as the
    /// namespace value (i.e. `env.{namespace}`) and STILL runs
    /// [`NativePlugin::register`] to attach callbacks on top of it. This
    /// lets a plugin ship a `#[v8_class]`-backed instance (with internal
    /// fields + methods + getters + Weak finalizers) as the namespace
    /// object while keeping its pre-existing flat callbacks working
    /// unchanged.
    ///
    /// Plumbing for the macro-driven DB namespace (Stage 1 of the
    /// `runtime-macros` refactor): `DbPlugin::build_instance` mints a
    /// `Db` v8_class instance, the runtime overlays the 27 existing
    /// `zeroship.db.*` callbacks on top, and SDK callers see no
    /// behavioural change. Future-facing methods (`collection`,
    /// `subscribe` returning a `Subscription` wrapper) are declared as
    /// `#[v8_method]` on the v8_class and don't have to be wired through
    /// `NativeRegistrar`.
    fn build_instance<'s>(
        &self,
        _scope: &mut v8::PinScope<'s, '_>,
        _app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        None
    }
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

    /// Register a native function as `env.{namespace}.{name}`.
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

    /// Push an arbitrary setup closure that runs once during
    /// `build_env_object`, with access to the active scope and the
    /// plugin's namespace object. Used by plugins that need to do
    /// side-effect setup beyond a single function registration —
    /// e.g. installing classes onto `globalThis` via their
    /// `Class::install(scope)` shape.
    ///
    /// The `name` argument is purely for debugging; the closure is
    /// applied by `build_env_object` exactly once after `register`
    /// returns.
    pub fn add_setup<F>(&mut self, name: &'static str, setup: F)
    where
        F: Fn(&mut v8::PinScope, v8::Local<v8::Object>) + 'static,
    {
        self.entries.push((name, Box::new(setup)));
    }
}

/// Build the `env` object that user code sees as:
///   - the return value of `__zs_env()`
///   - the `env` argument of `fetch(request, env, ctx)`
///   - the `env` named export of the `zeroship` module
///
/// Structure:
///   env = { ...<plugin namespaces>, ...<merged vars+secrets from env_json> }
///
/// Plugin namespaces are lowercase (matching the existing `NativePlugin::namespace()`
/// convention — "db", "kv", etc.). User scalars come from the `env_json`
/// snapshot's split `{ vars, secrets }` halves: vars are layered first,
/// then secrets override on key collision (secrets win because they are
/// the authoritative value for sensitive lookups). If a scalar name
/// collides with a plugin namespace, the plugin wins (platform primitives
/// override user config) — the overlay order below enforces this.
///
/// The returned object is shallow-frozen (via `Object.freeze`), so user code
/// can't monkey-patch `env.db = null` at runtime. Namespace sub-objects
/// remain mutable by reference, but their registered methods are attached as
/// own properties at build time — replacing them would require reassigning
/// through the frozen parent.
pub(crate) fn build_env_object(
    scope: &mut v8::PinScope,
    plugins: &[Arc<dyn NativePlugin>],
    env_json: &str,
) -> v8::Global<v8::Object> {
    // Start with the merged user scalars. `env_json` arrives in the
    // `{ vars, secrets, expose }` wire shape — flatten the two maps into
    // a single object with secrets winning on collision (the authoritative
    // sensitive value). Malformed JSON degrades to an empty object.
    let env_obj = v8::Object::new(scope);
    if let Ok(serde_json::Value::Object(root)) = serde_json::from_str::<serde_json::Value>(env_json) {
        // Vars first, then secrets — secrets win on overwrite.
        for half in ["vars", "secrets"] {
            if let Some(serde_json::Value::Object(map)) = root.get(half) {
                for (k, v) in map {
                    if let Some(s) = v.as_str() {
                        let k_v8 = v8::String::new(scope, k).unwrap();
                        let v_v8 = v8::String::new(scope, s).unwrap();
                        env_obj.set(scope, k_v8.into(), v_v8.into());
                    }
                }
            }
        }
    }

    // Overlay plugin namespaces. Each plugin contributes an object under
    // its declared namespace with all its registered callbacks. A second
    // plugin claiming the same namespace panics — platform misconfiguration
    // should fail loud, not silently shadow.
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

        let mut registrar = NativeRegistrar::new();
        plugin.register(&mut registrar);

        // Stage 1: if the plugin provides a custom namespace instance
        // (typically a `#[v8_class]`-backed object with internal fields
        // + methods + Weak finalizers), use it as the namespace value.
        // Otherwise fall back to a fresh `v8::Object` — the legacy
        // shape every plugin still relies on. The callbacks from
        // `register()` are layered on top in either case so existing
        // surfaces (`env.db.find`, `env.kv.get`, …) keep working
        // whether or not the plugin migrated to v8_class.
        //
        // `build_instance` reads the app_id from the runtime's
        // `SharedState` slot so plugins can stamp it onto the boxed
        // state at construction time (avoiding the per-callback
        // `env_vars.get("APP_ID")` lookup the legacy flat callbacks
        // do).
        let app_id_for_instance = scope
            .get_slot::<crate::state::SharedState>()
            .and_then(|s| s.borrow().env_vars.get("APP_ID").cloned())
            .unwrap_or_else(|| "default".to_string());
        let ns_obj = plugin
            .build_instance(scope, &app_id_for_instance)
            .unwrap_or_else(|| v8::Object::new(scope));
        for (_name, apply_fn) in &registrar.entries {
            apply_fn(scope, ns_obj);
        }

        let ns_key = v8::String::new(scope, ns_name).unwrap();
        env_obj.set(scope, ns_key.into(), ns_obj.into());
    }

    // Shallow freeze via Object.freeze. Prevents user code from reassigning
    // `env.db = null` or adding `env.foo`. Namespace sub-objects stay
    // unfrozen — their methods are already attached, and freezing them
    // would be a minor defensive-in-depth gain at the cost of breaking any
    // future plugin that expects to extend its namespace after registration.
    let freeze_source = "(obj) => Object.freeze(obj)";
    let code = v8::String::new(scope, freeze_source).unwrap();
    if let Some(script) = v8::Script::compile(scope, code, None)
        && let Some(func_val) = script.run(scope)
        && let Ok(freeze_fn) = v8::Local::<v8::Function>::try_from(func_val)
    {
        let undefined = v8::undefined(scope).into();
        let _ = freeze_fn.call(scope, undefined, &[env_obj.into()]);
    }

    v8::Global::new(scope, env_obj)
}
