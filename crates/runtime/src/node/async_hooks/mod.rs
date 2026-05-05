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
//! `globalThis.__zsAsyncHooks.AsyncLocalStorage` — the boundary
//! object the Vite-side synthetic `node:async_hooks` module
//! re-exports.

pub mod als;

/// Install `globalThis.__zsAsyncHooks` with the native classes that
/// back `node:async_hooks`. Called from `core/init.rs::setup_globals`
/// alongside the other native installs.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let obj = v8::Object::new(scope);
    als::install_on(scope, obj, "AsyncLocalStorage");
    let key = v8::String::new(scope, "__zsAsyncHooks").unwrap();
    global.set(scope, key.into(), obj.into());
}
