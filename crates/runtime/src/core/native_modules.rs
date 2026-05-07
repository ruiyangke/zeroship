//! Native synthetic ES modules — `node:*` resolved by the runtime.
//!
//! V8's `Module::create_synthetic_module` lets us mint an ESM record
//! whose exports are populated at evaluation time by a Rust callback,
//! so user code can `import { AsyncLocalStorage } from "node:async_hooks"`
//! and get the real native class — no `globalThis.__zsAsyncHooks`
//! handoff, no Vite-side virtual-module shim.
//!
//! Adding a module: append to `NATIVE_MODULES` and write a
//! `synthetic_module(scope) -> Local<Module>` that delegates to V8's
//! `create_synthetic_module` with the export names + an eval-steps
//! callback that calls `set_synthetic_module_export` for each name.
//!
//! Synthetic modules have no imports — the BFS in `modules.rs` skips
//! `get_module_requests()` walking for them.

#![allow(unsafe_code)]

/// Resolve a bare `node:*` specifier to a synthetic module, or `None`
/// if it's not one we own. Called from both the eager import-graph
/// walker and the V8 resolve callback.
pub fn resolve_native<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    specifier: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    match specifier {
        "node:async_hooks" => Some(crate::node::async_hooks::synthetic_module(scope)),
        "node:crypto" => Some(crate::node::crypto::synthetic_module(scope)),
        "node:zlib" => Some(crate::node::zlib::synthetic_module(scope)),
        "node:os" => Some(crate::node::os::synthetic_module(scope)),
        "node:path" => Some(crate::node::path::synthetic_module(scope)),
        _ => None,
    }
}

/// True if `specifier` is a runtime-owned native module — used to
/// short-circuit "Cannot resolve" errors before we surface them.
pub fn is_native(specifier: &str) -> bool {
    matches!(
        specifier,
        "node:async_hooks" | "node:crypto" | "node:zlib" | "node:os" | "node:path"
    )
}

/// Install `globalThis.__zeroshipNodeBuiltin(specifier)` — the dev-only
/// bridge that returns a runtime-native module's namespace object.
///
/// Vite's ModuleRunner can't issue native ESM imports in dev (each
/// module body is `eval`-wrapped, no static import graph). The
/// vite-plugin's `fetchModule` for `node:async_hooks` / `node:crypto`
/// returns a tiny re-export stub that calls this helper to source the
/// real exports. In prod (bundled .zship) the bare `import` survives
/// to V8 and goes through `resolve_native` — this helper isn't on the
/// hot path there.
pub fn install_global_bridge<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let f = v8::Function::new(scope, bridge_callback).unwrap();
    let key = v8::String::new(scope, "__zeroshipNodeBuiltin").unwrap();
    global.set(scope, key.into(), f.into());
}

fn bridge_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let msg = v8::String::new(scope, "__zeroshipNodeBuiltin: specifier required").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let specifier = args.get(0).to_rust_string_lossy(scope);
    let module = match resolve_native(scope, &specifier) {
        Some(m) => m,
        None => {
            let msg = v8::String::new(
                scope,
                &format!("__zeroshipNodeBuiltin: unknown specifier '{specifier}'"),
            )
            .unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    // Synthetic modules have no imports, so the resolve callback is
    // never invoked — pass a no-op that returns None.
    let ok = module.instantiate_module(scope, empty_resolve);
    if ok != Some(true) {
        let msg = v8::String::new(scope, "__zeroshipNodeBuiltin: instantiate failed").unwrap();
        let exc = v8::Exception::error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let _ = module.evaluate(scope);
    let ns = module.get_module_namespace();
    rv.set(ns);
}

/// No-op resolver used when instantiating modules that have no
/// imports — synthetic modules and the dynamic-import bridge below.
/// Public-in-crate so `dynamic_import.rs` can reuse it.
pub(crate) fn empty_resolve<'a>(
    _context: v8::Local<'a, v8::Context>,
    _specifier: v8::Local<'a, v8::String>,
    _import_attributes: v8::Local<'a, v8::FixedArray>,
    _referrer: v8::Local<'a, v8::Module>,
) -> Option<v8::Local<'a, v8::Module>> {
    None
}
