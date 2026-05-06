//! Native `node:async_hooks`.
//!
//! Currently exposes the only piece any production app actually
//! depends on — `AsyncLocalStorage`, the context primitive
//! `@langchain/langgraph` and friends use to thread per-request /
//! per-graph state across `await` boundaries.
//!
//! ## Why native (per ISS-01)
//!
//! The closure-based polyfill in `docs/reference/node-compat.md`'s
//! original §"node:async_hooks" reverted state synchronously in
//! `try { fn(...) } finally { ... }` and therefore tore down the
//! store before any awaited continuation resumed. That broke
//! `interrupt()` after `await model.invoke(...)` in a LangGraph
//! StateGraph (the visible symptom: "Called interrupt() outside
//! the context of a graph").
//!
//! V8 v147 ships an embedder-data slot that propagates ACROSS
//! every async hop — `Isolate::SetContinuationPreservedEmbedderData`.
//! `AsyncLocalStorage` here uses that slot to hold a JS Map of
//! `<per-instance Symbol> -> store`, so `getStore()` after an await
//! returns the value V8's continuation walker carried forward.
//!
//! ## JS surface
//!
//! ```js
//! import { AsyncLocalStorage } from "node:async_hooks";
//! ```
//!
//! Resolved by `core::native_modules::resolve_native` into a V8
//! `SyntheticModule` whose exports are populated lazily by
//! [`evaluate`] on first import.

#![allow(unsafe_code)]

pub mod als;

/// Mint a synthetic ESM record for `node:async_hooks`. Called by the
/// module loader's `resolve_native` when user code imports the
/// specifier. Exports are populated by [`evaluate`] at module
/// evaluation time.
pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Module> {
    let module_name = v8::String::new(scope, "node:async_hooks").unwrap();
    let export_names = [
        v8::String::new(scope, "AsyncLocalStorage").unwrap(),
        v8::String::new(scope, "AsyncResource").unwrap(),
        v8::String::new(scope, "createHook").unwrap(),
        v8::String::new(scope, "executionAsyncId").unwrap(),
        v8::String::new(scope, "triggerAsyncId").unwrap(),
        v8::String::new(scope, "executionAsyncResource").unwrap(),
        v8::String::new(scope, "asyncWrapProviders").unwrap(),
        v8::String::new(scope, "default").unwrap(),
    ];
    v8::Module::create_synthetic_module(scope, module_name, &export_names, evaluate)
}

/// SyntheticModule evaluation steps — populates exports on first
/// `import`. V8 invokes this once per module, after instantiation,
/// when the synthetic module's evaluation runs.
fn evaluate<'s>(
    context: v8::Local<'s, v8::Context>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    v8::callback_scope!(unsafe scope, context);

    // AsyncLocalStorage — the one real export. The other names are
    // throw-on-call stubs so npm packages that probe the surface (e.g.
    // tracing libraries asking for `executionAsyncId`) get a clear
    // error rather than `undefined is not a function`.
    //
    // Task #169: `run(store, fn, ...args)` is now wired via the macro
    // (variadic param support shipped in the same task). No manual
    // proto.set step here — `AsyncLocalStorage::install` does it all.
    let als_tmpl = als::AsyncLocalStorage::install(scope);
    let als_fn = als_tmpl.get_function(scope).unwrap();

    set_export(scope, module, "AsyncLocalStorage", als_fn.into());

    let async_resource = make_not_implemented_ctor(scope, "AsyncResource");
    set_export(scope, module, "AsyncResource", async_resource.into());

    let create_hook = not_implemented_fn(scope, "createHook");
    set_export(scope, module, "createHook", create_hook.into());
    let exec_id = not_implemented_fn(scope, "executionAsyncId");
    set_export(scope, module, "executionAsyncId", exec_id.into());
    let trig_id = not_implemented_fn(scope, "triggerAsyncId");
    set_export(scope, module, "triggerAsyncId", trig_id.into());
    let exec_res = not_implemented_fn(scope, "executionAsyncResource");
    set_export(scope, module, "executionAsyncResource", exec_res.into());

    let providers = v8::Object::new(scope);
    set_export(scope, module, "asyncWrapProviders", providers.into());

    // `import nh from "node:async_hooks"` shape — the default export is
    // a namespace object mirroring the named exports.
    let default = v8::Object::new(scope);
    let als_key = v8::String::new(scope, "AsyncLocalStorage").unwrap();
    default.set(scope, als_key.into(), als_fn.into());
    let ar_key = v8::String::new(scope, "AsyncResource").unwrap();
    default.set(scope, ar_key.into(), async_resource.into());
    set_export(scope, module, "default", default.into());

    Some(v8::undefined(scope).into())
}

fn set_export<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    module: v8::Local<v8::Module>,
    name: &str,
    value: v8::Local<'s, v8::Value>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let _ = module.set_synthetic_module_export(scope, key, value);
}

/// `class X { constructor() { throw Error("X is not implemented...") } }`
fn make_not_implemented_ctor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &'static str,
) -> v8::Local<'s, v8::Function> {
    // Inline a tiny JS factory — simpler than wiring a Rust constructor
    // template just to throw.
    let src = format!(
        r#"(function() {{ throw Object.assign(new Error("{name} is not implemented in zeroship's V8 runtime"), {{ code: "ERR_METHOD_NOT_IMPLEMENTED" }}); }})"#
    );
    let src_v8 = v8::String::new(scope, &src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let val = script.run(scope).unwrap();
    val.try_into().unwrap()
}

fn not_implemented_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &'static str,
) -> v8::Local<'s, v8::Function> {
    make_not_implemented_ctor(scope, name)
}
