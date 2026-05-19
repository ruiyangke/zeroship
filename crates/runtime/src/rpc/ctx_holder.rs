//! `RpcCtx` — a per-request native holder exposing lazy accessors for
//! `requestId`, `traceId`, `method`, `url`, `headers`, `signal`, `user`,
//! `idempotencyKey`. See `docs/perf/rpc-ctx-regression-2026-05-07.md` §4
//! S10 for the design rationale (avoids ~4 µs/request of eager Headers /
//! URL / AbortController construction on procedures that never read ctx).
//!
//! ## Identity invariants
//!
//! - `ctx.signal` returns the SAME JS object on every read within one
//!   request — the AbortController is minted eagerly in `mint_rpc_ctx`
//!   so `register_in_flight` can register it before user code runs;
//!   the signal V8 wrapper is extracted from the controller on first
//!   read and cached on the holder.
//! - `ctx.headers` and `ctx.url` are also cached — multiple reads return
//!   the same JS object (so `ctx.headers === ctx.headers` is `true`).
//!   Per the 2026-05-07 amendment to `docs/proposals/rpc.md` §3 these
//!   are NOT frozen — mutations succeed and are request-scoped.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::sync::Arc;

use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter};

use crate::state::OpError;

// ---------------------------------------------------------------------------
// RpcCtx state
// ---------------------------------------------------------------------------

/// Per-request platform context. Built fresh for every `default.rpc`
/// invocation. `user_json` carries the gateway-verified `ZeroShip-User`
/// payload as a JSON string — same format `zeroship.auth.getUser()`
/// already returns. `None` for `auth: "anon"` requests.
pub struct RpcCtx {
    pub request_id: String,
    pub trace_id: String,
    pub method: String,
    pub url: String,
    /// Stored as an `Arc` so the dispatch path can hand the same backing
    /// `Vec` to multiple consumers (the holder + future borrow-only paths)
    /// with a refcount-only clone instead of an O(N) Vec copy. The Vec
    /// itself is still materialized once at dispatch entry — this shape
    /// just removes the redundant clone the holder used to do.
    pub headers: Arc<Vec<(String, String)>>,
    pub user_json: Option<String>,
    pub idempotency_key: Option<String>,
    /// Eagerly-minted AbortController. Held as a `Global` so
    /// `register_in_flight` can clone it for the abort registry before
    /// user code runs — eviction must be able to fire abort even on
    /// procedures that never read `ctx.signal`.
    pub abort_controller: RefCell<Option<v8::Global<v8::Object>>>,
    /// Lazily-materialized `ctx.headers` wrapper. First read constructs
    /// a native Headers via `build_kernel_headers` and caches the Global.
    pub cached_headers: RefCell<Option<v8::Global<v8::Object>>>,
    /// Lazily-materialized `ctx.url` wrapper. First read invokes the
    /// global URL constructor and caches the result.
    pub cached_url: RefCell<Option<v8::Global<v8::Object>>>,
    /// Lazily-materialized `ctx.signal` wrapper. First read extracts the
    /// signal V8 object from `abort_controller`; cached for [SameObject].
    pub cached_signal: RefCell<Option<v8::Global<v8::Object>>>,
    /// Lazily-materialized `ctx.user` wrapper. JSON.parse is deferred
    /// until first read. `null` is materialized once on first call.
    pub cached_user: RefCell<Option<v8::Global<v8::Value>>>,
}

// ---------------------------------------------------------------------------
// RpcCtx IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
impl RpcCtx {
    /// `new RpcCtx()` — kernel-only. User JS never calls this; the
    /// macro requires a constructor for install codegen, so this just
    /// builds an empty placeholder. Real instances are constructed via
    /// [`mint_rpc_ctx`].
    #[v8_constructor]
    fn new() -> RpcCtx {
        RpcCtx {
            request_id: String::new(),
            trace_id: String::new(),
            method: String::new(),
            url: String::new(),
            headers: Arc::new(Vec::new()),
            user_json: None,
            idempotency_key: None,
            abort_controller: RefCell::new(None),
            cached_headers: RefCell::new(None),
            cached_url: RefCell::new(None),
            cached_signal: RefCell::new(None),
            cached_user: RefCell::new(None),
        }
    }

    #[v8_getter]
    #[v8_name = "requestId"]
    fn request_id_getter<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        v8::String::new(scope, &self.request_id)
            .map(|s| s.into())
            .unwrap_or_else(|| v8::undefined(scope).into())
    }

    #[v8_getter]
    #[v8_name = "traceId"]
    fn trace_id_getter<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        v8::String::new(scope, &self.trace_id)
            .map(|s| s.into())
            .unwrap_or_else(|| v8::undefined(scope).into())
    }

    #[v8_getter]
    #[v8_name = "method"]
    fn method_getter<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        v8::String::new(scope, &self.method)
            .map(|s| s.into())
            .unwrap_or_else(|| v8::undefined(scope).into())
    }

    #[v8_getter]
    #[v8_name = "idempotencyKey"]
    fn idempotency_key_getter<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match &self.idempotency_key {
            Some(k) => v8::String::new(scope, k)
                .map(|s| s.into())
                .unwrap_or_else(|| v8::undefined(scope).into()),
            None => v8::undefined(scope).into(),
        }
    }

    /// `ctx.user` — lazily JSON.parse the gateway payload on first read.
    #[v8_getter]
    #[v8_name = "user"]
    fn user_getter<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        if let Some(g) = self.cached_user.borrow().as_ref() {
            return v8::Local::new(scope, g.clone());
        }
        let val: v8::Local<v8::Value> = match &self.user_json {
            Some(j) => match v8::String::new(scope, j).and_then(|s| v8::json::parse(scope, s)) {
                Some(v) => v,
                None => v8::null(scope).into(),
            },
            None => v8::null(scope).into(),
        };
        *self.cached_user.borrow_mut() = Some(v8::Global::new(scope, val));
        val
    }

    /// `ctx.headers` — lazily mint a native Headers wrapper on first
    /// read; cache for the request's lifetime. Per the 2026-05-07 amendment
    /// the wrapper is mutable and request-scoped.
    #[v8_getter]
    #[v8_name = "headers"]
    fn headers_getter<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        if let Some(g) = self.cached_headers.borrow().as_ref() {
            return v8::Local::new(scope, g.clone()).into();
        }
        let obj = match crate::headers::build_kernel_headers(scope, self.headers.as_slice()) {
            Some(o) => o,
            None => return v8::undefined(scope).into(),
        };
        *self.cached_headers.borrow_mut() = Some(v8::Global::new(scope, obj));
        obj.into()
    }

    /// `ctx.url` — lazily construct a native URL on first read.
    #[v8_getter]
    #[v8_name = "url"]
    fn url_getter<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        if let Some(g) = self.cached_url.borrow().as_ref() {
            return v8::Local::new(scope, g.clone()).into();
        }
        let obj = match build_native_url(scope, &self.url) {
            Ok(o) => o,
            Err(_) => return v8::undefined(scope).into(),
        };
        *self.cached_url.borrow_mut() = Some(v8::Global::new(scope, obj));
        obj.into()
    }

    /// `ctx.signal` — extract the AbortSignal from the AbortController
    /// (minting one lazily if the caller didn't request eager construction);
    /// cache for [SameObject].
    #[v8_getter]
    #[v8_name = "signal"]
    fn signal_getter<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        if let Some(g) = self.cached_signal.borrow().as_ref() {
            return v8::Local::new(scope, g.clone()).into();
        }
        let existing = self.abort_controller.borrow().clone();
        let controller_g = match existing {
            Some(g) => g,
            None => {
                let c = match build_abort_controller(scope) {
                    Ok(c) => c,
                    Err(_) => return v8::undefined(scope).into(),
                };
                let g = v8::Global::new(scope, c);
                *self.abort_controller.borrow_mut() = Some(g.clone());
                g
            }
        };
        let controller = v8::Local::new(scope, controller_g);
        let s_key = match v8::String::new(scope, "signal") {
            Some(s) => s,
            None => return v8::undefined(scope).into(),
        };
        let signal_v = match controller.get(scope, s_key.into()) {
            Some(v) => v,
            None => return v8::undefined(scope).into(),
        };
        let signal_obj = match v8::Local::<v8::Object>::try_from(signal_v) {
            Ok(o) => o,
            Err(_) => return v8::undefined(scope).into(),
        };
        *self.cached_signal.borrow_mut() = Some(v8::Global::new(scope, signal_obj));
        signal_obj.into()
    }
}

// ---------------------------------------------------------------------------
// mint helpers
// ---------------------------------------------------------------------------

/// Per-isolate cache of the RpcCtx FunctionTemplate's prototype +
/// instance template. Both lookups dominate the per-request mint cost
/// when not cached (instance_template + get_function + prototype get
/// each trip through V8 for every dispatch).
///
/// Storage: `v8::Eternal<T>` rather than `v8::Global<T>`. Both fields
/// are set-once on first dispatch and read on every RPC dispatch
/// thereafter. The previous `Global` fields required
/// `slot.field.clone()` (= `v8__Global__New` — a fresh `GlobalHandles`
/// slot) on every call to drop the slot borrow before
/// `v8::Local::new`. Eternals are isolate-lifetime handles whose
/// `get(scope)` returns the `Local` directly without allocating.
/// Mirrors the `__BrandSlot_*` Eternal conversion in commit b08786a
/// and the `ResponseTemplateSlot` Eternal conversion in commit 6fa5422.
struct RpcCtxTemplateSlot {
    instance_tmpl: v8::Eternal<v8::ObjectTemplate>,
    prototype: v8::Eternal<v8::Value>,
}

fn get_or_init_template_slot<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> (v8::Local<'s, v8::ObjectTemplate>, v8::Local<'s, v8::Value>) {
    if let Some(slot) = scope.get_slot::<RpcCtxTemplateSlot>() {
        // Eternal::get materialises the Local without allocating a
        // fresh GlobalHandles slot — same access pattern as the macro
        // brand slots (b08786a) and ResponseTemplateSlot (6fa5422).
        // The slot is always populated below before `set_slot`, so
        // `get` returning None would be a runtime invariant violation;
        // fall through to the install path defensively.
        if let (Some(inst), Some(proto)) =
            (slot.instance_tmpl.get(scope), slot.prototype.get(scope))
        {
            return (inst, proto);
        }
    }
    let tmpl = RpcCtx::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    // Populate the Eternal handles before stashing the slot. `Eternal::set`
    // only needs `&scope`, but `scope.set_slot` needs `&mut scope` — the
    // Eternal storage is already populated by the time we hand it to
    // `set_slot`, so no prior borrow is held across the mutable call.
    let inst_e: v8::Eternal<v8::ObjectTemplate> = v8::Eternal::empty();
    inst_e.set(scope, inst_tmpl);
    let proto_e: v8::Eternal<v8::Value> = v8::Eternal::empty();
    proto_e.set(scope, proto_v);
    scope.set_slot(RpcCtxTemplateSlot {
        instance_tmpl: inst_e,
        prototype: proto_e,
    });
    (inst_tmpl, proto_v)
}

/// Mint a fresh `RpcCtx` JS wrapper on `scope` from the supplied Rust
/// state. Headers / URL / signal / user / abort-controller materialize
/// on first accessor read. When `eager_abort_controller` is true, the
/// AbortController is constructed up-front so the caller can hand the
/// Local to `register_in_flight` (multi-tenant worker path).
///
/// Returns `(holder, Option<controller>)`; the second slot is `None`
/// when `eager_abort_controller` is false.
pub fn mint_rpc_ctx<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request_id: String,
    trace_id: String,
    method: String,
    url: String,
    headers: Arc<Vec<(String, String)>>,
    user_json: Option<String>,
    idempotency_key: Option<String>,
    eager_abort_controller: bool,
) -> Result<(v8::Local<'s, v8::Object>, Option<v8::Local<'s, v8::Object>>), OpError> {
    let (inst_tmpl, proto_v) = get_or_init_template_slot(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error("RpcCtx instance allocation failed"))?;
    obj.set_prototype(scope, proto_v);

    let (eager_controller, controller_global) = if eager_abort_controller {
        let c = build_abort_controller(scope)?;
        let g = v8::Global::new(scope, c);
        (Some(c), Some(g))
    } else {
        (None, None)
    };

    let state = RpcCtx {
        request_id,
        trace_id,
        method,
        url,
        headers,
        user_json,
        idempotency_key,
        abort_controller: RefCell::new(controller_global),
        cached_headers: RefCell::new(None),
        cached_url: RefCell::new(None),
        cached_signal: RefCell::new(None),
        cached_user: RefCell::new(None),
    };
    let boxed: Box<RpcCtx> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut RpcCtx));
        }),
    );
    std::mem::forget(weak);

    Ok((obj, eager_controller))
}

/// Construct a native `URL` JS object via `new URL(href)`.
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

/// Mint a fresh `AbortController`.
fn build_abort_controller<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "AbortController").unwrap();
    let ctor_v = global
        .get(scope, key.into())
        .ok_or_else(|| OpError::type_error("globalThis.AbortController missing"))?;
    let ctor = v8::Local::<v8::Function>::try_from(ctor_v)
        .map_err(|_| OpError::type_error("globalThis.AbortController is not a function"))?;
    ctor.new_instance(scope, &[])
        .ok_or_else(|| OpError::type_error("AbortController construction failed"))
}
