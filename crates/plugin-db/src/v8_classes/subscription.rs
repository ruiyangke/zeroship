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
//! - `next()` → `Promise<SubscriptionEvent | null>` — resolves with
//!   the next event as a typed object (`change` / `resync` /
//!   `closed`) or `null` once the subscription has been closed and
//!   drained.
//! - `close()` — idempotent **synchronous** teardown; subsequent
//!   polls resolve `null`. The asymmetry vs `next()` (which is
//!   async) is intentional: closing is a local state flip on the
//!   broker entry and has no I/O.
//!
//! The SDK layer (`sdks/db/src/subscribe.ts`) wraps this into the
//! public `AsyncIterable<SubscriptionEvent>` shape consumed by user
//! `for await ... of` loops.

#![allow(unsafe_code)]

use std::cell::RefCell;

use zeroship_runtime::state::{JsonValue, OpError};
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_async_method, v8_constructor, v8_method};

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
    /// `new Subscription()` from JS rejects — real instances come from
    /// [`mint_subscription`] via `db.openSubscription(name)` /
    /// `collection.openSubscription()`, which registers the broker
    /// entry as part of minting.
    #[v8_constructor]
    fn new() -> Result<Subscription, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// Poll for the next event. Resolves with the typed event object
    /// (`{kind:"change",...}` / `{kind:"resync"}` / `{kind:"closed"}`)
    /// or real JS `null` once the subscription has been closed and
    /// drained.
    ///
    /// The polling loop uses `poll_fn` to register the broker's Waker
    /// — when an event is pushed (or the broker closes the entry) the
    /// runtime's spawned-op pump wakes the future and resolves the
    /// promise. Once a `{"kind":"closed"}` event is observed, the
    /// wrapper drops its inner broker reference; subsequent polls
    /// resolve `null` without re-entering the broker.
    #[v8_async_method]
    async fn next(&self) -> Result<JsonValue, OpError> {
        // Snapshot the broker subscription Rc so `.await` doesn't hold
        // a `RefCell` borrow. `BrokerSubscription` is `Clone` (it wraps
        // `Rc<RefCell<Inner>>`) so this is a refcount-only clone.
        let sub_opt: Option<BrokerSubscription> = self.inner.borrow().as_ref().cloned();
        let sub = match sub_opt {
            Some(s) => s,
            None => return Ok(JsonValue("null".into())),
        };

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

        if matches!(msg, SubscriptionMessage::Closed) {
            self.inner.borrow_mut().take();
        }

        Ok(JsonValue(broker::message_to_json(&msg)))
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
// MV-name refusal — SDK boundary gate (P2 PR 3)
// ---------------------------------------------------------------------------

/// Refuse `openSubscription` on materialised-view shadow collection
/// names.
///
/// MV refreshes write to `__zeroship_mv_<name>` tables. The CDC
/// dispatcher's relation filter (`is_filtered_relation` in
/// `backend/sqlite/cdc.rs` / the equivalent PG publication scope) drops
/// those writes before they reach the broker, so a subscription opened
/// on an MV shadow name would silently never fire. We refuse at the
/// SDK boundary so callers see a loud error rather than a permanently-
/// empty stream — per design §13.5 and the P2 PR 3 gate
/// `mv_subscribe_rejected_at_sdk`.
///
/// Returns `Some(OpError)` for refused names and `None` for OK names.
/// Used by both `Db::open_subscription` (path `db.openSubscription(...)`)
/// and `Collection::open_subscription` (path
/// `db.collection(...).openSubscription()`). The wire `code` is
/// `invalid_collection` — the same code
/// `query::QueryError::InvalidCollection` flows through
/// (`crate::error::From<QueryError> for DbError`), so SDK callers
/// branch on a single stable string.
pub(crate) fn refuse_mv_subscription(collection: &str) -> Option<OpError> {
    if collection.starts_with("__zeroship_mv_") {
        return Some(OpError::coded(
            "invalid_collection",
            format!(
                "openSubscription: collection \"{collection}\" is a materialised-view \
                 shadow table; subscribe to the source collection instead. MV refreshes \
                 are filtered upstream of the broker."
            ),
            Some(
                "Subscribe to the collection the view is derived from (or call \
                 db.materializedView(name).refresh() on demand and re-read).".to_string(),
            ),
        ));
    }
    None
}

#[cfg(test)]
mod mv_refusal_tests {
    //! Unit-level coverage for the MV-name refusal at the SDK boundary.
    //! The `tests/sqlite_integration.rs::mv_subscribe_rejected_at_sdk`
    //! gate pins the same invariant end-to-end; this module pins the
    //! pure-Rust predicate so a regression surfaces without compiling
    //! the integration target.

    use super::refuse_mv_subscription;

    #[test]
    fn refuses_zeroship_mv_prefix() {
        let err = refuse_mv_subscription("__zeroship_mv_orders")
            .expect("MV-prefixed name must refuse");
        // `OpError::coded` stamps `code` onto a `CodedError` variant —
        // we don't reach into the variant body here (it's exposed via
        // the JS bridge layer); the existence of `Some(_)` is the
        // unit invariant. The integration test pins `e.code` from JS.
        let _ = err;
    }

    #[test]
    fn accepts_user_collections() {
        assert!(refuse_mv_subscription("users").is_none());
        assert!(refuse_mv_subscription("orders").is_none());
        // Edge: a user table whose name happens to start with
        // `__zeroship_m` (but not `__zeroship_mv_`) must NOT refuse.
        // The underscore-after-`mv` is the discriminator.
        assert!(refuse_mv_subscription("__zeroship_metrics").is_none());
        // Edge: the bare prefix without a suffix is still refused —
        // a `db.materializedView("")` callsite would land here and we
        // want it loud, not silently delivering empty.
        assert!(refuse_mv_subscription("__zeroship_mv_").is_some());
    }
}

// ---------------------------------------------------------------------------
// mint_subscription — build a wrapper from a fresh broker entry
// ---------------------------------------------------------------------------

/// Mint a `Subscription` JS wrapper for the given `(app_id,
/// collection)` pair.
///
/// Allocates the JS object, looks up the class template and prototype,
/// and only THEN subscribes on the thread-local broker — so a `?`-
/// propagated error from any fallible V8 op (`new_instance`,
/// `get_function`, prototype lookup) returns before a broker entry
/// exists. If we registered the broker entry first, an error between
/// the subscribe call and the wrapper install would leak the broker
/// slot: `Broker::subscription_count()` only prunes entries whose
/// `is_closed()` is true, and `is_closed` is flipped exclusively by
/// the JS-side wrapper's `Drop`/finalizer — which never runs if the
/// wrapper was never constructed.
///
/// Once V8 alloc succeeds, the broker handle is moved into a fresh
/// `Subscription` Rust state, boxed, installed in V8 internal field
/// 0, and reclaimed by the Weak finalizer registered alongside.
/// `Subscription`'s `Drop` impl calls `close()` on the broker entry,
/// so a JS caller that drops the wrapper without calling `.close()`
/// still releases the broker slot when V8 collects.
///
/// This mirrors the `mint_rpc_ctx` pattern in
/// `crates/runtime/src/rpc/ctx_holder.rs`.
pub fn mint_subscription<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    // STEP 1 — fallible V8 alloc. Any `?` here returns BEFORE we touch
    // the broker, so the broker entry can never leak. Do every V8 op
    // that can return `None` up front; only after we've committed do
    // we subscribe.
    let class_tmpl = Subscription::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error("Subscription instance allocation failed"))?;

    // Set the prototype so methods (`next`, `close`) resolve from the
    // instance. Mirrors the `mint_rpc_ctx` shape.
    let class_fn = class_tmpl
        .get_function(scope)
        .ok_or_else(|| OpError::type_error("Subscription template missing function"))?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn
        .get(scope, proto_key.into())
        .ok_or_else(|| OpError::type_error("Subscription prototype missing"))?;
    obj.set_prototype(scope, proto_v);

    // STEP 2 — broker subscribe. From this point on the broker entry's
    // ONLY owner is the `Subscription` state we're about to install in
    // internal field 0. The remaining operations (`Box::new`,
    // `Box::into_raw`, `External::new`, `set_internal_field`,
    // `with_guaranteed_finalizer`) are infallible: V8 `External::new`
    // and `set_internal_field` cannot fail (the slot exists per
    // `Subscription::install`), and `with_guaranteed_finalizer` is
    // documented as infallible. So no `?` can run between here and the
    // wrapper being live.
    let broker_sub = broker::subscribe(app_id, collection);

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

#[cfg(test)]
mod tests {
    //! Regression guard for the broker-leak fix (code-critique r4
    //! 2026-05-22, M-NEW-2): if `broker::subscribe` runs BEFORE the
    //! fallible V8 alloc chain, any `?` propagation between the
    //! subscribe call and the wrapper install leaks a broker entry —
    //! `Broker::subscription_count` only prunes `is_closed()` slots,
    //! and `is_closed` is flipped exclusively by the JS-side wrapper's
    //! `Drop`/finalizer, which never runs if the wrapper was never
    //! constructed.
    //!
    //! Simulating V8 allocation failure deterministically in a unit
    //! test is impractical (we'd need to drive the isolate into OOM),
    //! so this is a structural assertion: the source text of
    //! `mint_subscription` must place `broker::subscribe(` AFTER every
    //! `?` operator in the function body. Future refactors that
    //! reintroduce the bug will trip this test.
    //!
    //! The integration test
    //! `tests/subscription_finalizer.rs::dropping_subscription_closes_broker_handle_on_gc`
    //! covers the happy path (V8 alloc succeeds → broker entry reclaimed
    //! on GC); this test covers the unhappy path's structural invariant.

    use crate::broker;

    const MINT_SUBSCRIPTION_SOURCE: &str = include_str!("subscription.rs");

    /// Locate the `mint_subscription` function body and return the
    /// substring between its opening `{` and the matching closing `}`.
    fn mint_subscription_body() -> &'static str {
        let needle = "pub fn mint_subscription<";
        let start = MINT_SUBSCRIPTION_SOURCE
            .find(needle)
            .expect("mint_subscription not found in source");
        let after_sig = &MINT_SUBSCRIPTION_SOURCE[start..];
        let open = after_sig.find('{').expect("no opening brace");
        // Walk the brace stack to find the matching close. Naive but
        // sufficient for this function (no string literals containing
        // unbalanced braces).
        let bytes = after_sig.as_bytes();
        let mut depth = 0i32;
        let mut i = open;
        while i < bytes.len() {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &after_sig[open + 1..i];
                    }
                }
                _ => {}
            }
            i += 1;
        }
        panic!("mint_subscription: unmatched braces");
    }

    #[test]
    fn mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure() {
        // Structural invariant: every `?` operator in
        // `mint_subscription` must appear BEFORE the
        // `broker::subscribe(` call. If a future refactor moves the
        // subscribe call back above any `?`, an error returned from
        // that `?` would leak a broker entry (Drop of the JS wrapper
        // never runs because the wrapper was never created, so the
        // broker's is_closed() filter never prunes it).
        let body = mint_subscription_body();
        let subscribe_pos = body
            .find("broker::subscribe(")
            .expect("broker::subscribe call not found in mint_subscription");

        // Find every `?` operator. Ignore `?` characters inside
        // doc/line comments and string literals — but in this function
        // there are no `?` chars in literals, only operators after V8
        // ops. The simplest correct scan: walk char-by-char and skip
        // `//`-to-newline ranges.
        let bytes = body.as_bytes();
        let mut i = 0;
        let mut question_positions: Vec<usize> = Vec::new();
        while i < bytes.len() {
            // Skip `//` line comments.
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i] == b'?' {
                question_positions.push(i);
            }
            i += 1;
        }

        assert!(
            !question_positions.is_empty(),
            "expected at least one `?` operator in mint_subscription; \
             test invariant is meaningless without one"
        );

        for q in &question_positions {
            assert!(
                *q < subscribe_pos,
                "found `?` operator at byte {q} AFTER broker::subscribe call \
                 at byte {subscribe_pos} in mint_subscription. This means a \
                 V8 alloc failure between subscribe() and the wrapper install \
                 would leak a broker entry. Reorder so every fallible V8 op \
                 runs BEFORE broker::subscribe."
            );
        }

        // Cross-check: the happy-path test below confirms the broker
        // sees the new entry only after V8 alloc commits.
    }

    #[test]
    fn mint_subscription_happy_path_registers_exactly_one_broker_entry() {
        // Sanity counterpart: when V8 alloc succeeds the function
        // DOES register a broker entry (so the structural test above
        // isn't accidentally passing by removing the subscribe call
        // altogether). Run in a fresh thread so we don't race with
        // other broker users on this test runner thread.
        let handle = std::thread::spawn(|| {
            assert_eq!(broker::live_subscription_count(), 0);
            zeroship_runtime::init_v8();
            let mut isolate = v8::Isolate::new(v8::CreateParams::default());
            v8::scope!(let handle_scope, &mut isolate);
            let context = v8::Context::new(handle_scope, Default::default());
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            {
                v8::scope!(let inner, scope);
                let _obj = super::mint_subscription(inner, "test_app_unit", "messages")
                    .expect("mint_subscription should succeed");
                assert_eq!(
                    broker::live_subscription_count(),
                    1,
                    "expected exactly one broker entry after mint"
                );
            }
            // Force GC so the finalizer reclaims the broker entry —
            // otherwise the thread-local broker carries an entry into
            // the (short) thread teardown and the cleanup-assertion
            // below would race the finalizer.
            scope.request_garbage_collection_for_testing(v8::GarbageCollectionType::Full);
            scope.perform_microtask_checkpoint();
            assert_eq!(broker::live_subscription_count(), 0);
        });
        handle.join().expect("thread panicked");
    }
}
