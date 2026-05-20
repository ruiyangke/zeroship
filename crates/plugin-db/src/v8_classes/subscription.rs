//! `Subscription` — native V8 wrapper around a `broker::Subscription`.
//!
//! The wrapper owns the `broker::Subscription` directly in its V8
//! internal field 0. A `v8::Weak::with_guaranteed_finalizer` registered
//! at construction calls `broker_sub.close()` when V8 collects the
//! wrapper, so callers that drop the JS handle without `.close()` still
//! release the broker slot.
//!
//! ## JS surface
//!
//! - `pollJson()` → `Promise<string | null>` — resolves with the next
//!   event's JSON envelope, or `null` once the subscription has been
//!   closed and drained.
//! - `close()` — idempotent synchronous teardown; subsequent polls
//!   resolve `null`.
//!
//! The SDK layer (`sdks/db/src/subscribe.ts`) wraps this into the
//! public `AsyncIterable<SubscriptionEvent>` shape consumed by user
//! `for await ... of` loops.
//!
//! `close()` is exposed as an idempotent synchronous method for
//! callers that want explicit teardown. Identical effect to dropping
//! the wrapper — both call `broker::Subscription::close()` — except
//! `close()` runs synchronously instead of waiting for GC.

#![allow(unsafe_code)]

use std::cell::RefCell;

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_method};

use crate::broker::{self, Subscription as BrokerSubscription, SubscriptionMessage};

// ---------------------------------------------------------------------------
// Subscription state
// ---------------------------------------------------------------------------

/// Owned state for one JS `Subscription` instance.
///
/// Field 0 of the wrapper holds a `Box<Subscription>` (this struct).
/// The Weak finalizer registered by `#[v8_class]` reclaims the Box on
/// GC and our manual `Drop` impl closes the broker handle — so dropping
/// the JS wrapper without an explicit `.close()` still reclaims the
/// broker slot.
#[derive(Debug)]
pub struct Subscription {
    /// The broker entry this wrapper owns. `RefCell` so `close()` can
    /// take the inner subscription without an `&mut self` method
    /// signature on `Drop` (`Drop` only gets `&mut self`, but the V8
    /// macro stores the Box behind a `*const Self` recovered as `&Self`).
    inner: RefCell<Option<BrokerSubscription>>,
}

impl Drop for Subscription {
    /// GC-time cleanup. Fires from the Weak finalizer the `#[v8_class]`
    /// macro registers on every instance: when V8 collects the wrapper
    /// object, the finalizer drops the Box, which runs this. Closing
    /// the broker subscription is idempotent — explicit `close()` runs
    /// it first, and this becomes a no-op on the second pass.
    fn drop(&mut self) {
        if let Some(sub) = self.inner.borrow_mut().take() {
            sub.close();
        }
    }
}

// ---------------------------------------------------------------------------
// Subscription IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)] // Methods invoked via V8 callbacks; Rust can't trace through extern.
impl Subscription {
    /// Construct an empty placeholder. Real instances are minted via
    /// [`mint_subscription`] — the macro requires a constructor for
    /// install codegen, so this exists to satisfy that contract.
    /// Calling `new Subscription()` from JS produces a wrapper whose
    /// `inner` is `None`; every method returns the "closed/drained"
    /// answer because there is no broker entry.
    #[v8_constructor]
    fn new() -> Subscription {
        Subscription {
            inner: RefCell::new(None),
        }
    }

    /// Poll for the next event. Resolves with the JSON-serialised
    /// [`SubscriptionMessage`] (matching the wire format
    /// `broker::message_to_json` produces) or `null` once the
    /// subscription has been closed and drained.
    ///
    /// The polling loop uses `poll_fn` to register the broker's Waker
    /// — when an event is pushed (or the broker closes the entry) the
    /// runtime's spawned-op pump wakes the future and resolves the
    /// promise.
    ///
    /// Once a `{"kind":"closed"}` event is observed, the wrapper drops
    /// its inner broker reference; subsequent polls resolve with `null`
    /// without re-entering the broker.
    #[zeroship_runtime_macros::v8_async_method]
    async fn poll_json(&self) -> Result<String, OpError> {
        // Snapshot the broker subscription Rc so `.await` doesn't hold
        // a `RefCell` borrow. `BrokerSubscription` is `Clone` (it wraps
        // `Rc<RefCell<Inner>>`) so this is a refcount-only clone.
        let sub_opt: Option<BrokerSubscription> = self.inner.borrow().as_ref().cloned();
        let sub = match sub_opt {
            Some(s) => s,
            None => return Ok("null".into()),
        };

        // Polling loop — drain via poll_fn so the broker's
        // `wake_by_ref` re-schedules us. Mirrors the existing
        // `callbacks::subscribe_poll` shape so behaviour is identical
        // between the handle-id API and this wrapper.
        let msg = std::future::poll_fn(|cx| {
            if let Some(m) = sub.pop() {
                return std::task::Poll::Ready(m);
            }
            if sub.is_closed() && sub.is_terminal() {
                return std::task::Poll::Ready(SubscriptionMessage::Closed);
            }
            sub.register_waker(cx.waker().clone());
            // Re-check in case a push raced with register_waker.
            if let Some(m) = sub.pop() {
                return std::task::Poll::Ready(m);
            }
            std::task::Poll::Pending
        })
        .await;

        // On `Closed`, release the broker handle — future polls
        // observe `null` without re-entering the broker.
        if matches!(msg, SubscriptionMessage::Closed) {
            self.inner.borrow_mut().take();
        }

        Ok(broker::message_to_json(&msg))
    }

    /// Idempotent synchronous close. Equivalent to dropping the
    /// wrapper, except the broker entry is released immediately
    /// instead of at GC time.
    #[v8_method]
    fn close(&self) {
        if let Some(sub) = self.inner.borrow_mut().take() {
            sub.close();
        }
    }
}

// ---------------------------------------------------------------------------
// mint_subscription — build a wrapper from a fresh broker entry
// ---------------------------------------------------------------------------

/// Mint a `Subscription` JS wrapper for the given `(app_id,
/// collection)` pair.
///
/// Subscribes on the thread-local broker, boxes the resulting
/// `BrokerSubscription` inside a fresh `Subscription` Rust state, and
/// installs the Box in V8 internal field 0 of a new instance of the
/// `Subscription` class. The Weak finalizer registered alongside calls
/// our `Drop` impl on GC, which closes the broker entry — so a JS
/// caller that drops the wrapper without calling `.close()` still
/// releases the broker slot when V8 collects.
///
/// This mirrors the `mint_rpc_ctx` pattern in
/// `crates/runtime/src/rpc/ctx_holder.rs`.
pub fn mint_subscription<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    // Register on the broker first — failing the V8 allocation after
    // the broker registration would leak a broker entry. The
    // wrapper-allocation path below never registers a broker entry
    // until it has the JS object in hand.
    let broker_sub = broker::subscribe(app_id, collection);

    let class_tmpl = Subscription::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error("Subscription instance allocation failed"))?;

    // Set the prototype so methods (`poll_json`, `close`) resolve from
    // the instance. Mirrors the `mint_rpc_ctx` shape.
    let class_fn = class_tmpl
        .get_function(scope)
        .ok_or_else(|| OpError::type_error("Subscription template missing function"))?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn
        .get(scope, proto_key.into())
        .ok_or_else(|| OpError::type_error("Subscription prototype missing"))?;
    obj.set_prototype(scope, proto_v);

    let state = Subscription {
        inner: RefCell::new(Some(broker_sub)),
    };
    let boxed: Box<Subscription> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<Subscription>`;
    // the finalizer closure casts back to the same type and drops the
    // Box exactly once when V8 reclaims the wrapper. `Subscription`'s
    // `Drop` impl closes the broker entry.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Subscription));
        }),
    );
    std::mem::forget(weak);

    Ok(obj)
}
