//! Native `AsyncLocalStorage` per Node.js `node:async_hooks`.
//!
//! Replaces the closure-based polyfill described in
//! `docs/reference/node-compat.md`'s old §"node:async_hooks" — which
//! reverted state synchronously in `try { fn(...) } finally { ... }`
//! and therefore tore down the store before any awaited continuation
//! resumed (ISS-01).
//!
//! ## Storage layout
//!
//! - **Per-instance Symbol** (`v8::Global<v8::Symbol>`) — keys this
//!   ALS into the per-isolate context Map. A fresh Symbol is minted
//!   in the constructor; identity is stable for the wrapper's lifetime.
//!
//! - **Per-isolate context Map** — a JS `v8::Map` stored in V8's
//!   `ContinuationPreservedEmbedderData` slot. V8 propagates this
//!   slot AUTOMATICALLY across every async hop: `await`, microtask,
//!   `.then`, generator yield, native-Promise resolution. That is
//!   the entire point of the slot — embedders use it precisely so
//!   that "context across await" works without a PromiseHook.
//!
//! ## `run(store, fn, ...args)` semantics
//!
//! 1. Read the current map from the slot. If absent (slot is
//!    `undefined`), allocate a fresh empty Map.
//! 2. Clone it. We allocate per call because the slot value is
//!    captured by reference inside any awaited Promise's continuation;
//!    mutating it in place would leak the new entry into other
//!    sibling-async branches that branched off before this `run`.
//! 3. Set `<our-symbol> -> store` on the clone.
//! 4. Install the clone in the slot.
//! 5. Invoke `fn(...args)`. Use `tc_scope` to catch JS exceptions so
//!    we can restore the slot value before re-throwing.
//! 6. Restore the previous slot value (the unmodified pre-clone Map,
//!    or `undefined` if there was none) BEFORE returning to user code.
//!
//! Critical: the slot must be restored on ANY exit path — synchronous
//! return, JS-thrown exception, or even a Promise rejection bubbled up
//! through awaits. The latter is taken care of automatically by V8's
//! own slot-propagation: when control resumes in a different
//! continuation, the slot already carries the value V8 captured for
//! that continuation, so we don't need to do anything special.
//!
//! ## `enterWith(store)` / `disable()`
//!
//! These don't restore. `enterWith` is a non-reversible "set this
//! store as the current one"; `disable` removes our entry from the
//! current map (or no-ops if absent). LangChain uses `enterWith` in
//! some setup paths.

#![allow(unsafe_code)]

use std::cell::RefCell;

use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method};

// ---------------------------------------------------------------------------
// AsyncLocalStorage state
// ---------------------------------------------------------------------------

/// Backing state for an AsyncLocalStorage JS wrapper. Each instance
/// holds a unique Symbol used as the key into the per-isolate context
/// Map (stored in V8's `ContinuationPreservedEmbedderData`).
pub struct AsyncLocalStorage {
    /// Per-instance unique key. Stored as a `Global<Symbol>` so we
    /// can look it up against the slot's Map without re-minting a
    /// new Symbol on every read. JS user code never sees this Symbol;
    /// it's purely the internal key.
    pub(crate) key: RefCell<Option<v8::Global<v8::Symbol>>>,
}

impl Default for AsyncLocalStorage {
    fn default() -> Self {
        AsyncLocalStorage {
            key: RefCell::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// AsyncLocalStorage IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
impl AsyncLocalStorage {
    /// `new AsyncLocalStorage()` — Node.js `node:async_hooks` API.
    /// Mints a fresh Symbol used to key this instance's stores into
    /// the per-isolate context Map.
    #[v8_constructor]
    fn new(scope: &mut v8::PinScope) -> AsyncLocalStorage {
        let desc = v8::String::new(scope, "zs:AsyncLocalStorage").unwrap();
        let sym = v8::Symbol::new(scope, Some(desc));
        let als = AsyncLocalStorage::default();
        *als.key.borrow_mut() = Some(v8::Global::new(scope, sym));
        als
    }

    /// `als.getStore()` — read the current store for this ALS
    /// instance. Looks up our Symbol in the context Map stored in
    /// V8's `ContinuationPreservedEmbedderData`; returns `undefined`
    /// when not inside a `run()` (or the entry was removed via
    /// `disable()`).
    #[v8_method]
    #[v8_name = "getStore"]
    fn get_store<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        let map = match read_context_map(scope) {
            Some(m) => m,
            None => return v8::undefined(scope).into(),
        };
        let key_global = match self.key.borrow().clone() {
            Some(g) => g,
            None => return v8::undefined(scope).into(),
        };
        let key_local = v8::Local::new(scope, key_global);
        match map.get(scope, key_local.into()) {
            Some(v) => v,
            None => v8::undefined(scope).into(),
        }
    }

    /// `als.enterWith(store)` — set this instance's store on the
    /// current context, with NO restore. The new value is visible
    /// for the rest of this synchronous frame and any continuations
    /// that branch off after this point.
    ///
    /// LangChain uses this in some bootstrap paths (e.g. setting a
    /// singleton runnable-config at module init).
    #[v8_method]
    #[v8_name = "enterWith"]
    fn enter_with(&self, scope: &mut v8::PinScope, store: v8::Local<v8::Value>) {
        let map = ensure_context_map(scope);
        let key_global = match self.key.borrow().clone() {
            Some(g) => g,
            None => return,
        };
        let key_local = v8::Local::new(scope, key_global);
        map.set(scope, key_local.into(), store);
        scope.set_continuation_preserved_embedder_data(map.into());
    }

    /// `als.disable()` — remove this instance's store from the
    /// current context. No restore. After this, `getStore()` on the
    /// same instance returns `undefined` for the rest of the frame.
    #[v8_method]
    fn disable(&self, scope: &mut v8::PinScope) {
        let map = match read_context_map(scope) {
            Some(m) => m,
            None => return,
        };
        let key_global = match self.key.borrow().clone() {
            Some(g) => g,
            None => return,
        };
        let key_local = v8::Local::new(scope, key_global);
        map.delete(scope, key_local.into());
    }
}

// ---------------------------------------------------------------------------
// Slot helpers
// ---------------------------------------------------------------------------

/// Read the current per-isolate context Map from V8's
/// `ContinuationPreservedEmbedderData` slot. Returns `None` when the
/// slot is `undefined` (initial state) or holds something that isn't
/// a Map (defensive — embedders shouldn't mix uses but we tolerate it).
pub(crate) fn read_context_map<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<v8::Local<'s, v8::Map>> {
    let val = scope.get_continuation_preserved_embedder_data();
    if val.is_undefined() || val.is_null() {
        return None;
    }
    v8::Local::<v8::Map>::try_from(val).ok()
}

/// Read the current context Map, or allocate a fresh empty Map and
/// install it in the slot.
pub(crate) fn ensure_context_map<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Map> {
    if let Some(m) = read_context_map(scope) {
        return m;
    }
    let m = v8::Map::new(scope);
    scope.set_continuation_preserved_embedder_data(m.into());
    m
}

/// Clone a JS Map by walking its entries. V8's Map.as_array yields
/// `[k0, v0, k1, v1, ...]` flattened pairs (per spec).
pub(crate) fn clone_map<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    src: v8::Local<v8::Map>,
) -> v8::Local<'s, v8::Map> {
    let dst = v8::Map::new(scope);
    let arr = src.as_array(scope);
    let len = arr.length();
    let mut i: u32 = 0;
    while i + 1 < len {
        let k = match arr.get_index(scope, i) {
            Some(v) => v,
            None => return dst,
        };
        let v = match arr.get_index(scope, i + 1) {
            Some(v) => v,
            None => return dst,
        };
        dst.set(scope, k, v);
        i += 2;
    }
    dst
}

// ---------------------------------------------------------------------------
// Hand-rolled `run(store, fn, ...args)` callback
// ---------------------------------------------------------------------------
//
// Variadic JS args don't fit the `#[v8_method]` macro's positional
// extraction, so the `run` callback is hand-rolled. It's installed on
// the prototype after `AsyncLocalStorage::install()` returns
// (see `install_global` below).

#[inline]
fn als_state_from_obj<'a>(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<&'a AsyncLocalStorage> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *mut AsyncLocalStorage;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: brand check has already verified `obj` is an
    // AsyncLocalStorage wrapper; the boxed state's lifetime is tied
    // to the wrapper via the macro-installed weak finalizer.
    Some(unsafe { &*ptr })
}

/// `run(store, fn, ...args)` — invoke `fn(...args)` with the store
/// bound to this ALS for the duration of the call AND for every
/// continuation that branches off from inside `fn`. The previous
/// slot value is restored on every exit path (return, throw,
/// rejection).
pub(crate) fn run_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Brand check — exposed by the macro as `__zs_is_AsyncLocalStorage`.
    let this_v: v8::Local<v8::Value> = args.this().into();
    if !__zs_is_AsyncLocalStorage(scope, this_v) {
        let msg = v8::String::new(scope, "Illegal invocation").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let this = args.this();
    let als = match als_state_from_obj(scope, this) {
        Some(s) => s,
        None => {
            let msg = v8::String::new(scope, "Illegal invocation").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let store = args.get(0);
    let fn_arg = args.get(1);
    let fn_val: v8::Local<v8::Function> = match fn_arg.try_into() {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "AsyncLocalStorage.run: callback must be callable").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    // Collect varargs (positions 2..N).
    let arg_len = args.length();
    let extra_count = if arg_len > 2 { (arg_len - 2) as usize } else { 0 };
    let mut call_args: Vec<v8::Local<v8::Value>> = Vec::with_capacity(extra_count);
    for i in 0..extra_count {
        call_args.push(args.get((i + 2) as i32));
    }

    // Snapshot the previous slot value so we can restore it on every
    // exit path. The slot starts as `undefined` in fresh isolates;
    // stashing as a Global keeps it alive across the call regardless
    // of GC.
    let prev_slot = scope.get_continuation_preserved_embedder_data();
    let prev_slot_global = v8::Global::new(scope, prev_slot);

    // Build the new context Map = clone(current) + (this.key -> store).
    let next_map = match read_context_map(scope) {
        Some(m) => clone_map(scope, m),
        None => v8::Map::new(scope),
    };
    let key_global = match als.key.borrow().clone() {
        Some(g) => g,
        None => {
            // No key minted — should be impossible (constructor sets
            // it). Treat as "no-op run" so the user's fn still runs.
            let undefined = v8::undefined(scope).into();
            match fn_val.call(scope, undefined, &call_args) {
                Some(v) => rv.set(v),
                None => {} // exception is already pending
            }
            return;
        }
    };
    let key_local = v8::Local::new(scope, key_global);
    next_map.set(scope, key_local.into(), store);
    scope.set_continuation_preserved_embedder_data(next_map.into());

    // Invoke fn(...args) inside a TryCatch so we can restore the
    // slot before re-throwing. Using `tc_scope!` borrows an inner
    // scope; we capture the outcome and let the inner scope drop
    // before touching the outer scope again.
    let (result_global, exc_global) = {
        v8::tc_scope!(let tc, scope);
        let undefined = v8::undefined(tc).into();
        let r = fn_val.call(tc, undefined, &call_args);
        if tc.has_caught() {
            let exc = tc.exception().map(|e| v8::Global::new(tc, e));
            (None, exc)
        } else {
            (r.map(|v| v8::Global::new(tc, v)), None)
        }
    };

    // Restore the slot — UNCONDITIONAL. This is the whole point of
    // the function's documented critical contract.
    let prev_slot_local = v8::Local::new(scope, &prev_slot_global);
    scope.set_continuation_preserved_embedder_data(prev_slot_local);

    if let Some(exc_g) = exc_global {
        let exc_local = v8::Local::new(scope, &exc_g);
        scope.throw_exception(exc_local);
        return;
    }
    if let Some(r_g) = result_global {
        let r_local = v8::Local::new(scope, &r_g);
        rv.set(r_local);
    }
}

// ---------------------------------------------------------------------------
// Install the run() method on the AsyncLocalStorage prototype
// ---------------------------------------------------------------------------

/// Install `AsyncLocalStorage` as a constructor function on `target`
/// under the name `name`. Also installs the hand-rolled `run` method
/// on the prototype (the macro doesn't support varargs, so `run` lives
/// outside the `#[v8_class]` impl block).
pub fn install_on<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    target: v8::Local<v8::Object>,
    name: &str,
) {
    let tmpl = AsyncLocalStorage::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    // Install run() on the prototype.
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();
    let run_fn = v8::Function::new(scope, run_callback).unwrap();
    let run_key = v8::String::new(scope, "run").unwrap();
    proto.set(scope, run_key.into(), run_fn.into());

    let key = v8::String::new(scope, name).unwrap();
    target.set(scope, key.into(), class_fn.into());
}
