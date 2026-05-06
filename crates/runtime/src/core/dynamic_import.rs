//! V8 host callback for `await import(specifier)`.
//!
//! Bundle-resident only. Three resolution paths:
//!
//!   1. Registry hit — module already pre-compiled by `load_modules`'s
//!      static-import BFS, OR previously cached by an earlier dynamic
//!      import. Returned as-is so static + dynamic imports of the same
//!      specifier share a single module instance (no double evaluation,
//!      no split namespace).
//!   2. Native synthetic (`node:async_hooks`, `node:crypto`) — minted
//!      via `native_modules::resolve_native`, instantiated, evaluated,
//!      then cached into the registry so future dynamic OR static
//!      imports of the same specifier hit path 1.
//!   3. Miss — reject with `TypeError("Cannot find module '<spec>'")`.
//!      No fetch, no compile-on-demand: the bundle is the closed world.
//!
//! V8's per-module evaluation cache makes `module.evaluate()` idempotent
//! after the first call, so the registry's module handles are safe to
//! return repeatedly without re-running side effects.
//!
//! Hooked from `RuntimeInner::new_with_plugins` via
//! `set_host_import_module_dynamically_callback` — must be installed
//! before any user JS runs so the very first `import()` hits this path.

#![allow(unsafe_code)]

use crate::core::modules::SharedRegistry;
use crate::core::native_modules;

/// Variants tried for an unknown specifier — same set as the static
/// `resolve_callback` so dynamic and static specifier shapes resolve to
/// the same compiled module.
fn registry_lookup<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    spec: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    let registry = scope.get_slot::<SharedRegistry>()?.clone();
    let reg = registry.borrow();
    let candidates = [
        spec.to_string(),
        spec.strip_prefix("./").unwrap_or(spec).to_string(),
        format!("{spec}.js"),
        format!("{}.js", spec.strip_prefix("./").unwrap_or(spec)),
    ];
    for candidate in &candidates {
        if let Some(g) = reg.get(candidate) {
            return Some(v8::Local::new(scope, g));
        }
    }
    None
}

/// Cache a freshly-minted native synthetic into the registry under its
/// bare specifier so a subsequent dynamic OR static import of
/// `node:foo` hits the same module record.
fn cache_into_registry<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    spec: &str,
    module: v8::Local<'s, v8::Module>,
) {
    if let Some(registry) = scope.get_slot::<SharedRegistry>() {
        let reg = registry.clone();
        let g = v8::Global::new(scope, module);
        reg.borrow_mut().insert(spec.to_string(), g);
    }
}

/// Walk a fresh module to `Evaluated`. Synthetic modules have no
/// imports so the empty resolver suffices; bundle modules retrieved
/// from the registry are already at least `Instantiated` (and usually
/// `Evaluated`) thanks to `load_modules`'s eager Phase 3+4 — the
/// status guards below short-circuit duplicate work.
fn instantiate_and_evaluate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    if module.get_status() == v8::ModuleStatus::Uninstantiated {
        let _ = module.instantiate_module(scope, native_modules::empty_resolve);
    }
    if module.get_status() == v8::ModuleStatus::Instantiated {
        let _ = module.evaluate(scope);
    }
    if module.get_status() == v8::ModuleStatus::Errored {
        return None;
    }
    Some(module.get_module_namespace())
}

/// Reject `resolver` with `TypeError(message)` and return its promise.
fn reject_typeerror<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    resolver: v8::Local<'s, v8::PromiseResolver>,
    message: String,
) -> v8::Local<'s, v8::Promise> {
    let msg = v8::String::new(scope, &message).unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    let promise = resolver.get_promise(scope);
    resolver.reject(scope, exc);
    promise
}

/// Reject with whatever exception V8 left on the module (its
/// `get_exception()` for `Errored`) — gives the user the real cause
/// rather than a flattened "evaluation failed" string.
fn reject_module_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    resolver: v8::Local<'s, v8::PromiseResolver>,
    module: v8::Local<'s, v8::Module>,
) -> v8::Local<'s, v8::Promise> {
    let exc = module.get_exception();
    let promise = resolver.get_promise(scope);
    resolver.reject(scope, exc);
    promise
}

/// `set_host_import_module_dynamically_callback` target. Returns
/// `None` only on `PromiseResolver::new` failure (stack overflow, OOM)
/// — every other path settles a promise and returns `Some`.
pub(crate) fn host_import_module_dynamically_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    _resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let spec = specifier.to_rust_string_lossy(scope);

    // Path 1: registry hit. Covers static-imported bundle modules AND
    // any native synthetic that an earlier static or dynamic import
    // already minted+cached.
    if let Some(module) = registry_lookup(scope, &spec) {
        match instantiate_and_evaluate(scope, module) {
            Some(ns) => {
                let promise = resolver.get_promise(scope);
                resolver.resolve(scope, ns);
                return Some(promise);
            }
            None => return Some(reject_module_error(scope, resolver, module)),
        }
    }

    // Path 2: native synthetic not yet seen. Mint, walk to Evaluated,
    // cache so subsequent imports hit path 1.
    if let Some(module) = native_modules::resolve_native(scope, &spec) {
        match instantiate_and_evaluate(scope, module) {
            Some(ns) => {
                cache_into_registry(scope, &spec, module);
                let promise = resolver.get_promise(scope);
                resolver.resolve(scope, ns);
                return Some(promise);
            }
            None => return Some(reject_module_error(scope, resolver, module)),
        }
    }

    // Path 3: not in the bundle.
    Some(reject_typeerror(
        scope,
        resolver,
        format!("Cannot find module '{spec}'"),
    ))
}
