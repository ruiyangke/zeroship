//! V8 host callback for `await import(specifier)`.
//!
//! Resolution paths, in order:
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
//!   2.5. Runtime-provided module (`@zeroship/bootstrap/install-schema`,
//!      `@zeroship/db/internal`, `zeroship`) — see `bootstrap_modules`.
//!      The runtime injects the code that imports these (the bootstrap
//!      `runtime-entry.js`), so it owns their resolution even when the
//!      tree-shaken `.zship` bundle doesn't carry them (ISS-63). The
//!      module + its transitive runtime-provided deps are compiled into
//!      the registry, instantiated through the real static-graph
//!      resolver, evaluated, and cached for path 1.
//!   3. Miss — reject with `TypeError("Cannot find module '<spec>'")`.
//!      No fetch, no compile-on-demand for arbitrary bundle paths: outside
//!      the runtime-provided set, the bundle is the closed world.
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
/// `Evaluated`) thanks to `load_modules`'s eager instantiate/evaluate
/// path. The status guards below short-circuit duplicate work.
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

/// Resolve a runtime-provided module (`@zeroship/bootstrap/install-schema`,
/// `@zeroship/db/internal`, `zeroship`) — see [`bootstrap_modules`].
///
/// The runtime, not the bundle, owns these: it injects the code that
/// imports them (`runtime-entry.js` spliced into the bootstrap `index.js`),
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

/// Instantiate (via the real static-graph resolver, so transitive imports
/// resolve) and evaluate a runtime-provided bootstrap module, returning its
/// namespace. Distinct from [`instantiate_and_evaluate`] (which uses
/// `empty_resolve` for import-less synthetics): bootstrap modules DO have
/// static imports.
fn instantiate_and_evaluate_bootstrap<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    if module.get_status() == v8::ModuleStatus::Uninstantiated {
        let _ = module.instantiate_module(scope, modules::resolve_callback);
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

    // Path 2.5: runtime-provided module (`@zeroship/bootstrap/install-schema`,
    // `@zeroship/db/internal`, `zeroship`). The runtime injects the code
    // that imports these (runtime-entry.js), so it owns their resolution
    // even when the tree-shaken bundle doesn't carry them (ISS-63). The
    // requested module is cached into the registry by
    // `resolve_bootstrap_module`, so subsequent imports hit path 1.
    if let Some(module) = resolve_bootstrap_module(scope, &spec) {
        match instantiate_and_evaluate_bootstrap(scope, module) {
            Some(ns) => {
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
