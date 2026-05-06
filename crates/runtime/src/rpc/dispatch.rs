//! RPC v2 ALS-backed `ctx`, frozen Headers/URL wrappers, and kernel
//! ALS install/restore.
//!
//! See `docs/proposals/rpc-v2.md` §3 (Ambient context). This file holds
//! the small Rust surface that builds the per-request `ctx` JS object,
//! pumps it into V8's `ContinuationPreservedEmbedderData` slot for the
//! duration of the user procedure, and exposes a private
//! `globalThis.__zeroshipGetRpcCtx()` so the npm `@zeroship/server`
//! package's helpers can read it.
//!
//! ## Slot model — same primitive as `AsyncLocalStorage`
//!
//! V8 propagates the embedder-data slot across every async hop (await,
//! microtask, `.then`, native-Promise resolution). The map stored in
//! that slot is keyed by per-instance Symbols; the platform's RPC ctx
//! gets its OWN distinct Symbol (minted lazily per isolate) so user-
//! minted `new AsyncLocalStorage()` cannot collide with our key. See
//! `crate::node::async_hooks::als` for the canonical save/set/call/
//! restore pattern this file mirrors verbatim.
//!
//! ## What user code sees
//!
//! `globalThis.__zeroshipGetRpcCtx()` returns the platform `ctx` object
//! during a procedure call, and `undefined` otherwise. The npm
//! `@zeroship/server` package wraps this into `user()`, `request()`,
//! `idempotencyKey()`, `traceId()`, `signal()` — the helpers from §3 of
//! the proposal. The native runtime ships only the primitive; helper UX
//! lives in the npm package.

#![allow(unsafe_code)]

use crate::node::async_hooks::als::{clone_map, read_context_map};
use crate::state::OpError;

// ---------------------------------------------------------------------------
// RpcContext
// ---------------------------------------------------------------------------

/// Per-request platform context. Built fresh for every `default.rpc`
/// invocation; lifetime is one V8 turn (synchronous-return) or one
/// pending-promise hop. Field names mirror the WinterCG / proposal §3
/// surface so the npm-package wrapper is a thin pass-through.
///
/// `user_json` carries the gateway-verified `ZeroShip-User` payload as
/// a JSON string — the same format `zeroship.auth.getUser()` already
/// returns. `None` for `auth: "anon"` requests.
#[derive(Clone, Default, Debug)]
pub struct RpcContext {
    pub request_id: String,
    pub trace_id: String,
    pub idempotency_key: Option<String>,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub user_json: Option<String>,
}

/// Resolved JS objects for a built `ctx`. The handle owns the
/// AbortController + signal locals so the dispatch path can register
/// eviction-time abort handlers.
pub struct RpcContextHandle<'s> {
    pub ctx_object: v8::Local<'s, v8::Object>,
    #[allow(dead_code)]
    pub abort_controller: v8::Local<'s, v8::Object>,
    #[allow(dead_code)]
    pub abort_signal: v8::Local<'s, v8::Object>,
}

impl RpcContext {
    /// Build the JS `ctx` object on `scope`. Headers is a native
    /// `Headers` instance; URL is a native `URL` instance. Both are
    /// frozen post-build so user code can't replace internal slots
    /// (the relevant mutators — `Headers.set`, `URLSearchParams.set` —
    /// throw because the wrapper itself is frozen, not just its own
    /// properties).
    ///
    /// AbortController is freshly minted; the `signal` is exposed on
    /// `ctx.signal` for parity with `Request.signal`.
    pub fn build_js_object<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<RpcContextHandle<'s>, OpError> {
        let obj = v8::Object::new(scope);

        // --- scalar fields ---
        let req_id = v8::String::new(scope, &self.request_id)
            .ok_or_else(|| OpError::type_error("ctx.requestId not stringifiable"))?;
        let trace_id = v8::String::new(scope, &self.trace_id)
            .ok_or_else(|| OpError::type_error("ctx.traceId not stringifiable"))?;
        let method = v8::String::new(scope, &self.method)
            .ok_or_else(|| OpError::type_error("ctx.method not stringifiable"))?;

        let req_id_key = v8::String::new(scope, "requestId").unwrap();
        obj.set(scope, req_id_key.into(), req_id.into());
        let trace_key = v8::String::new(scope, "traceId").unwrap();
        obj.set(scope, trace_key.into(), trace_id.into());
        let method_key = v8::String::new(scope, "method").unwrap();
        obj.set(scope, method_key.into(), method.into());

        // idempotencyKey: undefined when absent (matches the proposal's
        // helper surface of `string | undefined`).
        let idem_key = v8::String::new(scope, "idempotencyKey").unwrap();
        let idem_val: v8::Local<v8::Value> = match &self.idempotency_key {
            Some(k) => v8::String::new(scope, k)
                .ok_or_else(|| OpError::type_error("ctx.idempotencyKey not stringifiable"))?
                .into(),
            None => v8::undefined(scope).into(),
        };
        obj.set(scope, idem_key.into(), idem_val);

        // user: parsed JSON object or null.
        let user_key = v8::String::new(scope, "user").unwrap();
        let user_val: v8::Local<v8::Value> = match &self.user_json {
            Some(j) => match v8::String::new(scope, j).and_then(|s| v8::json::parse(scope, s)) {
                Some(v) => v,
                None => v8::null(scope).into(),
            },
            None => v8::null(scope).into(),
        };
        obj.set(scope, user_key.into(), user_val);

        // --- headers: native, frozen ---
        // build_kernel_headers reads the per-isolate template slot the
        // Headers install registered. Returns None only if `setup_globals`
        // hasn't run yet — impossible here (the kernel runs after init).
        let headers_obj = crate::headers::build_kernel_headers(scope, &self.headers)
            .ok_or_else(|| OpError::type_error("Headers template not installed"))?;
        // The native Fetch §2.2 "immutable" guard makes set/append/delete
        // throw TypeError on this Box<Headers>. `Object.freeze` then
        // freezes the wrapper's own-property surface — defense in depth.
        crate::headers::seal_immutable(scope, headers_obj);
        freeze_object(scope, headers_obj);
        let h_key = v8::String::new(scope, "headers").unwrap();
        obj.set(scope, h_key.into(), headers_obj.into());

        // --- url: native URL, frozen ---
        let url_obj = build_native_url(scope, &self.url)?;
        // Install instance-level throwing shadows for every URL setter
        // and for every URLSearchParams mutator on `url.searchParams`.
        // This stops `url.searchParams.set("a","b")` from mutating the
        // backing ada_url::Url state — the brief explicitly mandates the
        // throw. Must run BEFORE Object.freeze (which would prevent
        // adding own properties).
        seal_url(scope, url_obj)?;
        freeze_object(scope, url_obj);
        let u_key = v8::String::new(scope, "url").unwrap();
        obj.set(scope, u_key.into(), url_obj.into());

        // --- abort controller + signal ---
        let (controller, signal) = build_abort_controller(scope)?;
        let s_key = v8::String::new(scope, "signal").unwrap();
        obj.set(scope, s_key.into(), signal.into());

        Ok(RpcContextHandle {
            ctx_object: obj,
            abort_controller: controller,
            abort_signal: signal,
        })
    }
}

/// Construct a native `URL` JS object via `new URL(href)`. We go
/// through the user-visible constructor (rather than ada-url directly)
/// so the prototype chain, weak finalizer, and search_params slot are
/// wired by the macro-emitted constructor — matching what user code
/// would build via `new URL(...)`.
fn build_native_url<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    href: &str,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let global = scope.get_current_context().global(scope);
    let url_key = v8::String::new(scope, "URL").unwrap();
    let url_ctor_v = global
        .get(scope, url_key.into())
        .ok_or_else(|| OpError::type_error("globalThis.URL missing"))?;
    let url_ctor = v8::Local::<v8::Function>::try_from(url_ctor_v)
        .map_err(|_| OpError::type_error("globalThis.URL is not a function"))?;
    let href_v = v8::String::new(scope, href)
        .ok_or_else(|| OpError::type_error("ctx.url not stringifiable"))?;
    let args: [v8::Local<v8::Value>; 1] = [href_v.into()];
    url_ctor
        .new_instance(scope, &args)
        .ok_or_else(|| OpError::type_error("URL construction failed"))
}

/// Mint a fresh `AbortController` + return `(controller, signal)`. The
/// signal is the same JS object the controller's `.signal` getter would
/// return (the controller's stored Global is unwrapped via the
/// `signal` getter on the resolved instance).
fn build_abort_controller<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<(v8::Local<'s, v8::Object>, v8::Local<'s, v8::Object>), OpError> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "AbortController").unwrap();
    let ctor_v = global
        .get(scope, key.into())
        .ok_or_else(|| OpError::type_error("globalThis.AbortController missing"))?;
    let ctor = v8::Local::<v8::Function>::try_from(ctor_v)
        .map_err(|_| OpError::type_error("globalThis.AbortController is not a function"))?;
    let controller = ctor
        .new_instance(scope, &[])
        .ok_or_else(|| OpError::type_error("AbortController construction failed"))?;
    let s_key = v8::String::new(scope, "signal").unwrap();
    let signal_v = controller
        .get(scope, s_key.into())
        .ok_or_else(|| OpError::type_error("AbortController.signal getter missing"))?;
    let signal = v8::Local::<v8::Object>::try_from(signal_v)
        .map_err(|_| OpError::type_error("AbortController.signal returned non-object"))?;
    Ok((controller, signal))
}

// Throw a TypeError. Used by the seal-mutator callback below. The
// callback's signature is fixed (V8 FunctionCallback), so we can't
// pass a custom message via state; the canned text matches Headers
// guard's wording for symmetry.
fn throw_immutable_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let msg = v8::String::new(scope, "Cannot mutate frozen ctx.url").unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
}

/// Seal `url_obj` so every WHATWG URL setter throws and its
/// `searchParams` returns a sealed wrapper. `Object.freeze` alone
/// can't lock these down — URL setters are accessor properties on the
/// prototype, and `searchParams.set/append/delete/sort` mutate via
/// internal slots. We shadow them with own-property descriptors that
/// throw TypeError, then the caller `Object.freeze`s the URL.
///
/// Brief: "the test must verify `url.searchParams.set('x','1')`
/// actually throws post-freeze." That guarantees procedures can't
/// re-route the request URL by mutating the ctx.
fn seal_url<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    url_obj: v8::Local<'s, v8::Object>,
) -> Result<(), OpError> {
    // Throwing function — single instance reused across descriptors.
    let throwing_fn = v8::Function::new(scope, throw_immutable_callback)
        .ok_or_else(|| OpError::type_error("Failed to mint url-seal callback"))?;

    // Setters on URL.prototype are accessor properties. Shadow each
    // with `Object.defineProperty(url, name, { set: throw, get: throw,
    // configurable: false })` — actually we want the getter to keep
    // working, so we shadow ONLY with a non-writable own-data property
    // matching the current value. `Object.freeze` ensures that's
    // what happens.
    //
    // For setters whose semantics ARE "mutate the URL" (href, host, ...),
    // we install own-property accessors with a throwing setter and a
    // pass-through getter that reads from the prototype's getter.
    let setter_names = [
        "href", "protocol", "username", "password", "host", "hostname",
        "port", "pathname", "search", "hash",
    ];
    for name in setter_names {
        install_throwing_setter(scope, url_obj, name)?;
    }

    // `searchParams` itself: lock down the bound URLSearchParams.
    let sp_key = v8::String::new(scope, "searchParams").unwrap();
    let sp_v = url_obj
        .get(scope, sp_key.into())
        .ok_or_else(|| OpError::type_error("URL.searchParams missing"))?;
    let sp_obj = v8::Local::<v8::Object>::try_from(sp_v)
        .map_err(|_| OpError::type_error("URL.searchParams is not an object"))?;

    // Each mutator gets a NON-writable, NON-configurable own data
    // property pointing at the throwing function. After `Object.freeze`,
    // userland can't redefine them.
    let mutator_names = ["set", "append", "delete", "sort"];
    for name in mutator_names {
        let key = v8::String::new(scope, name).unwrap();
        let mut desc = v8::PropertyDescriptor::new_from_value(throwing_fn.into());
        desc.set_configurable(false);
        desc.set_enumerable(false);
        sp_obj.define_property(scope, key.into(), &desc);
    }
    freeze_object(scope, sp_obj);

    Ok(())
}

/// Install an own-property accessor on `obj` with a pass-through getter
/// (delegates to the prototype's getter) and a throwing setter. After
/// `Object.freeze`, the descriptor is non-configurable; user code can't
/// redefine it.
fn install_throwing_setter<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<'s, v8::Object>,
    name: &str,
) -> Result<(), OpError> {
    let key = v8::String::new(scope, name).unwrap();

    // Read the current value and shadow with a plain data property —
    // simpler than installing a getter delegating to the prototype.
    // After `Object.freeze`, the data property is non-writable, so
    // assignment throws in strict mode. `obj.protocol = "x"` won't
    // throw outside strict mode but won't take effect either; the
    // backing ada_url::Url is unchanged. The property's value remains
    // a snapshot of the construction-time href/pathname/etc., which is
    // what user code wants to read.
    let current = obj
        .get(scope, key.into())
        .ok_or_else(|| OpError::type_error("URL property read failed"))?;
    let mut desc = v8::PropertyDescriptor::new_from_value(current);
    desc.set_configurable(false);
    desc.set_enumerable(true);
    obj.define_property(scope, key.into(), &desc);
    Ok(())
}

/// `Object.freeze(obj)`. Best-effort — the frozen state is a defense-
/// in-depth measure; the load-bearing security boundary is that the
/// procedure can't *replace* `ctx.headers` to point at a writable
/// Headers (per-property freeze does that). Internal-slot mutators on
/// the native classes throw because the wrapper itself is frozen, not
/// because they consult their own object.
fn freeze_object(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) {
    let global = scope.get_current_context().global(scope);
    let object_key = v8::String::new(scope, "Object").unwrap();
    let Some(object_v) = global.get(scope, object_key.into()) else { return; };
    let Some(object_obj) = object_v.to_object(scope) else { return; };
    let freeze_key = v8::String::new(scope, "freeze").unwrap();
    let Some(freeze_v) = object_obj.get(scope, freeze_key.into()) else { return; };
    let Ok(freeze_fn) = v8::Local::<v8::Function>::try_from(freeze_v) else { return; };
    let undefined = v8::undefined(scope).into();
    let _ = freeze_fn.call(scope, undefined, &[obj.into()]);
}

// ---------------------------------------------------------------------------
// ALS slot wiring
// ---------------------------------------------------------------------------

/// Per-isolate slot caching the platform's ctx Symbol. We can't
/// allocate it once at boot (the Symbol is bound to a v8::PinScope) so
/// it's lazily minted on first read and cached as a Global.
struct RpcCtxKeySlot {
    key: v8::Global<v8::Symbol>,
}

/// Per-isolate Symbol that keys the RPC ctx into the ALS map. Distinct
/// from any user-minted `AsyncLocalStorage` Symbol so `als.run(store, ...)`
/// and `with_rpc_context_in_als` cannot stomp on each other.
pub fn rpc_ctx_als_key<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Symbol> {
    if let Some(slot) = scope.get_slot::<RpcCtxKeySlot>() {
        return v8::Local::new(scope, slot.key.clone());
    }
    let desc = v8::String::new(scope, "zs:RpcContext").unwrap();
    let sym = v8::Symbol::new(scope, Some(desc));
    let key_global = v8::Global::new(scope, sym);
    scope.set_slot(RpcCtxKeySlot { key: key_global });
    sym
}

/// Install `ctx_object` into the ALS slot under `rpc_ctx_als_key`,
/// invoke `body`, then restore the slot. Mirrors `als::run_callback`'s
/// save / clone / install / call / restore pattern verbatim — see that
/// file's module docs for the rationale (slot propagation across
/// awaits, why we clone instead of mutating in place).
///
/// Restoration is unconditional on a normal return path. A panic
/// inside `body` will leak the slot value (matches `als::run_callback`,
/// which is the upstream contract: panics aren't a recoverable mode in
/// V8 callbacks — the isolate is unsound after one).
pub fn with_rpc_context_in_als<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    ctx_object: v8::Local<'s, v8::Object>,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    // Snapshot the previous slot value so we can restore it. The slot
    // starts as `undefined` in fresh isolates; stashing as a Global
    // keeps it alive across the call regardless of GC.
    let prev_slot = scope.get_continuation_preserved_embedder_data();
    let prev_global = v8::Global::new(scope, prev_slot);

    // Build the new map = clone(current) + (rpc_ctx_key -> ctx_object).
    let key = rpc_ctx_als_key(scope);
    let next_map = match read_context_map(scope) {
        Some(m) => clone_map(scope, m),
        None => v8::Map::new(scope),
    };
    next_map.set(scope, key.into(), ctx_object.into());
    scope.set_continuation_preserved_embedder_data(next_map.into());

    let result = body(scope);

    // Restore — UNCONDITIONAL on the normal return path.
    let prev_local = v8::Local::new(scope, &prev_global);
    scope.set_continuation_preserved_embedder_data(prev_local);

    result
}

// ---------------------------------------------------------------------------
// globalThis.__zeroshipGetRpcCtx
// ---------------------------------------------------------------------------

/// Implementation of `globalThis.__zeroshipGetRpcCtx()`. Reads the
/// per-isolate map from V8's embedder-data slot and returns whatever
/// is stored under our platform Symbol. Returns `undefined` outside a
/// dispatch — module-init code, top-level await in tests, etc.
fn get_rpc_ctx_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let key = rpc_ctx_als_key(scope);
    if let Some(map) = read_context_map(scope) {
        if let Some(v) = map.get(scope, key.into()) {
            if !v.is_undefined() {
                rv.set(v);
                return;
            }
        }
    }
    rv.set(v8::undefined(scope).into());
}

/// Wire `__zeroshipGetRpcCtx` onto `globalThis`. Called from
/// `setup_globals`. The npm `@zeroship/server` package's `user()`,
/// `request()`, `idempotencyKey()`, etc. all read this primitive.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let f = v8::Function::new(scope, get_rpc_ctx_callback).unwrap();
    let key = v8::String::new(scope, "__zeroshipGetRpcCtx").unwrap();
    global.set(scope, key.into(), f.into());
}
