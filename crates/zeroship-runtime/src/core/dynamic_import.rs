//! V8 host callback for `await import(specifier)`.
//!
//! Bundle modules resolve relative to their importing module and compile lazily
//! with their static dependency closure. Static and dynamic imports share the
//! same module records. Imports settle after module evaluation, including
//! top-level await, finishes. Native and framework modules use the same registry;
//! an unknown module rejects without fetching code outside the bundle.
//!
//! V8's per-module evaluation cache makes `module.evaluate()` idempotent
//! after the first call, so the registry's module handles are safe to
//! return repeatedly without re-running side effects.
//!
//! Hooked from `RuntimeInner::new_with_plugins` via
//! `set_host_import_module_dynamically_callback` — must be installed
//! before any user JS runs so the very first `import()` hits this path.

#![allow(unsafe_code)]

use crate::core::bootstrap_modules;
use crate::core::modules::{self, SharedRegistry};
use crate::core::native_modules;

/// Cache a freshly-minted native synthetic into the registry under its
/// bare specifier so a subsequent dynamic OR static import of
/// `node:foo` hits the same module record.
fn cache_into_registry<'s>(
    scope: &v8::PinScope<'s, '_>,
    spec: &str,
    module: v8::Local<'s, v8::Module>,
) {
    if let Some(registry) = scope.get_slot::<SharedRegistry>() {
        let reg = registry.clone();
        let g = v8::Global::new(scope, module);
        reg.borrow_mut().insert(spec.to_string(), g);
    }
}

/// Resolve a runtime-provided module (`@zeroship/bootstrap/install-schema`,
/// `@zeroship/db/internal`, `zeroship`) — see [`bootstrap_modules`].
///
/// The runtime, not the bundle, owns these: it injects the code that
/// imports them (`runtime-entry.js` spliced into the host bootstrap),
/// so it must guarantee they resolve regardless of what the tree-shaken
/// `.zship` carries (ISS-63).
///
/// Strategy: compile the requested module AND its transitive
/// runtime-provided dependency closure into the per-isolate registry, then
/// instantiate with the SAME [`modules::resolve_callback`] the static graph
/// uses — so the bootstrap module's own `import ... from "@zeroship/db/internal"`
/// / `"zeroship"` lines resolve against the registry we just populated. The
/// requested module is left cached in the registry, so a later static OR
/// dynamic import of the same specifier hits path 1.
///
/// Returns `None` if `spec` isn't a runtime-provided module (caller falls
/// through to the not-found rejection) or if compilation fails (treated as
/// a not-found miss — a compile error here means the embedded dist drifted,
/// which the resolution tests catch).
fn resolve_bootstrap_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    spec: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    if !bootstrap_modules::is_bootstrap_module(spec) {
        return None;
    }
    let registry = scope.get_slot::<SharedRegistry>()?.clone();

    // Walk the transitive runtime-provided closure (DFS) and compile each
    // member into the registry if absent. `zeroship` is frequently already
    // present (the user app statically imports it); install-schema /
    // internal are not. Compiling a member doesn't evaluate it — that
    // happens during the entry's `instantiate_module` + `evaluate` below,
    // exactly as `load_modules` does for the static graph.
    let mut stack: Vec<&str> = vec![spec];
    let mut seen: Vec<&str> = Vec::new();
    while let Some(cur) = stack.pop() {
        if seen.contains(&cur) {
            continue;
        }
        seen.push(cur);

        let already = registry.borrow().get(cur).is_some();
        if !already {
            let source = bootstrap_modules::source_for(cur)?;
            let module = modules::compile_module(scope, cur, source).ok()?;
            registry.borrow_mut().insert(cur.to_string(), module);
        }
        for dep in bootstrap_modules::deps_of(cur) {
            stack.push(dep);
        }
    }

    // Hand back the requested module. The host callback instantiates it
    // through `modules::resolve_callback`, which now finds every transitive
    // import in the registry.
    let g = registry.borrow().get(spec)?.clone();
    Some(v8::Local::new(scope, &g))
}

/// Reject `resolver` with `TypeError(message)` and return its promise.
fn reject_typeerror<'s>(
    scope: &v8::PinScope<'s, '_>,
    resolver: v8::Local<'s, v8::PromiseResolver>,
    message: &str,
) -> v8::Local<'s, v8::Promise> {
    let msg = v8::String::new(scope, message).unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    let promise = resolver.get_promise(scope);
    resolver.reject(scope, exc);
    promise
}

/// Reject with whatever exception V8 left on the module (its
/// `get_exception()` for `Errored`) — gives the user the real cause
/// rather than a flattened "evaluation failed" string.
fn reject_module_error<'s>(
    scope: &v8::PinScope<'s, '_>,
    resolver: v8::Local<'s, v8::PromiseResolver>,
    module: v8::Local<'s, v8::Module>,
) -> v8::Local<'s, v8::Promise> {
    let exc = module.get_exception();
    let promise = resolver.get_promise(scope);
    resolver.reject(scope, exc);
    promise
}

/// Resolve an import after evaluation, including top-level await, has finished.
fn finish_import<'s>(
    scope: &v8::PinScope<'s, '_>,
    resolver: v8::Local<'s, v8::PromiseResolver>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Promise>> {
    if module.get_status() == v8::ModuleStatus::Uninstantiated
        && module.instantiate_module(scope, modules::resolve_callback) != Some(true)
    {
        return None;
    }
    if module.get_status() == v8::ModuleStatus::Errored {
        return Some(reject_module_error(scope, resolver, module));
    }
    let evaluation = module.evaluate(scope)?;
    let namespace = module.get_module_namespace();
    let Ok(evaluation) = v8::Local::<v8::Promise>::try_from(evaluation) else {
        // Native synthetic callbacks may complete synchronously without a promise.
        if !module.is_source_text_module() {
            resolver.resolve(scope, namespace);
            return Some(resolver.get_promise(scope));
        }
        return None;
    };
    let fulfilled = v8::Function::builder(return_namespace)
        .data(namespace)
        .build(scope)?;
    let settled = evaluation.then(scope, fulfilled)?;
    resolver.resolve(scope, settled.into());
    Some(resolver.get_promise(scope))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "V8's callback ABI passes its arguments by value"
)]
fn return_namespace(
    _scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut result: v8::ReturnValue,
) {
    result.set(args.data());
}

/// Compile dynamic bundle imports through the same resolver as static imports.
pub(crate) fn host_import_module_dynamically_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let spec = specifier.to_rust_string_lossy(scope);
    let referrer = resource_name.to_rust_string_lossy(scope);
    v8::tc_scope!(let scope, scope);
    let module = match modules::dynamic_module(scope, &spec, &referrer) {
        Ok(Some(module)) => Some(v8::Local::new(scope, &module)),
        Ok(None) => {
            if let Some(module) = native_modules::resolve_native(scope, &spec) {
                cache_into_registry(scope, &spec, module);
                Some(module)
            } else {
                resolve_bootstrap_module(scope, &spec)
            }
        }
        Err(error) => {
            if let Some(exception) = scope.exception() {
                resolver.reject(scope, exception);
                return Some(resolver.get_promise(scope));
            }
            return Some(reject_typeerror(scope, resolver, &error));
        }
    };
    if let Some(module) = module
        && let Some(promise) = finish_import(scope, resolver, module)
    {
        return Some(promise);
    }
    if let Some(exception) = scope.exception() {
        resolver.reject(scope, exception);
        return Some(resolver.get_promise(scope));
    }
    Some(reject_typeerror(
        scope,
        resolver,
        &format!("Cannot find module '{spec}'"),
    ))
}
