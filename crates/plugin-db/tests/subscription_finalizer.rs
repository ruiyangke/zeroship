//! Stage 3 — Subscription v8_class GC finalizer test.
//!
//! Closes the P8a handle-leak: when a JS caller drops the wrapper
//! without calling `.close()`, the Weak finalizer registered by
//! `mint_subscription` must still close the broker handle so the
//! broker slot is reclaimed on the next GC cycle.
//!
//! The test:
//!   1. Mints a `Subscription` wrapper bound to an in-process broker
//!      entry (subscription_count == 1).
//!   2. Lets the V8 Local reference go out of scope so V8 has no
//!      strong handles to the wrapper.
//!   3. Forces a major GC via
//!      `request_garbage_collection_for_testing`.
//!   4. Asserts `broker::live_subscription_count() == 0`.
//!
//! Before this PR the handle-id-based registry (`SUBSCRIPTIONS` in
//! `callbacks.rs`) had no way to detect that the JS-side AsyncIterable
//! had been dropped without `.return()`, so this would have stayed
//! at 1 indefinitely.

#![allow(unsafe_code)]

use zeroship_plugin_db::broker;
use zeroship_plugin_db::v8_classes::subscription::mint_subscription;
use zeroship_runtime::init_v8;

#[test]
fn dropping_subscription_closes_broker_handle_on_gc() {
    // Make sure other tests didn't leave a stray entry on this thread's
    // broker. The broker is thread-local, and Cargo runs tests on
    // worker threads, but this single test runs in isolation in its
    // own #[test] fn — its thread is fresh, so this is a sanity assert
    // rather than a teardown.
    assert_eq!(
        broker::live_subscription_count(),
        0,
        "broker not clean at test start"
    );

    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    // Mint the wrapper inside an inner scope so the Local is dropped
    // before we force GC. The wrapper's only strong reference is the
    // Local on the scope's handle stack — drop the scope and the Local
    // is gone too.
    {
        v8::scope!(let inner, scope);
        let _wrapper =
            mint_subscription(inner, "test_app", "messages").expect("mint_subscription");
        // Sanity: broker registered an entry.
        assert_eq!(
            broker::live_subscription_count(),
            1,
            "broker did not see the new subscription"
        );
        // _wrapper drops here when `inner` scope ends.
    }

    // V8 needs explicit GC to fire the finalizer in test mode. The
    // `request_garbage_collection_for_testing` path is gated behind
    // `--expose-gc` (set by `init_v8`) and synchronously runs Mark-
    // Compact, queuing finalizers. `perform_microtask_checkpoint`
    // drains the finalizer queue (V8 dispatches guaranteed finalizers
    // on the microtask scheduling tick).
    scope.request_garbage_collection_for_testing(v8::GarbageCollectionType::Full);
    scope.perform_microtask_checkpoint();

    // The finalizer ran -> `Subscription::drop` closed the broker
    // entry -> the broker's next sweep (via `subscription_count`'s
    // `is_closed` filter) reports 0.
    assert_eq!(
        broker::live_subscription_count(),
        0,
        "GC finalizer did not close the broker subscription"
    );
}

/// Counterpart: explicit `Subscription::close()` immediately releases
/// the broker entry, no GC required. Verifies the synchronous-close
/// path stays usable for callers that don't want to rely on GC.
#[test]
fn explicit_close_releases_broker_handle_synchronously() {
    assert_eq!(
        broker::live_subscription_count(),
        0,
        "broker not clean at test start"
    );

    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let wrapper = mint_subscription(scope, "test_app2", "messages2")
        .expect("mint_subscription");
    assert_eq!(
        broker::live_subscription_count(),
        1,
        "broker did not see the new subscription"
    );

    // Call .close() via the JS surface — exercise the actual
    // `#[v8_method]` callback, not the Rust-internal path.
    let close_key = v8::String::new(scope, "close").unwrap();
    let close_v = wrapper
        .get(scope, close_key.into())
        .expect("close prop missing");
    let close_fn: v8::Local<v8::Function> = close_v
        .try_into()
        .expect("close is not a function");
    close_fn.call(scope, wrapper.into(), &[]);

    assert_eq!(
        broker::live_subscription_count(),
        0,
        ".close() did not release the broker subscription"
    );

    // Idempotent: a second close() should be a no-op (still 0).
    close_fn.call(scope, wrapper.into(), &[]);
    assert_eq!(broker::live_subscription_count(), 0);
}
