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
//! - `ready()` -> `Promise<void>` - resolves only after CDC can receive
//!   changes. Postgres waits through publication/slot provisioning and a
//!   successful `START_REPLICATION` handshake.
//! - `next()` → `Promise<SubscriptionEvent | null>` — resolves with
//!   the next event as a typed object (`change` / `resync` /
//!   `closed`) or `null` once the subscription has been closed and
//!   drained.
//! - `close()` — idempotent **synchronous** teardown; subsequent
//!   polls resolve `null`. The asymmetry vs `next()` (which is
//!   async) is intentional: closing is a local state flip on the
//!   broker entry and has no I/O.
//!
//! The SDK layer (`packages/db/src/subscribe.ts`) wraps this into the
//! public `AsyncIterable<SubscriptionEvent>` shape consumed by user
//! `for await ... of` loops.

#![allow(unsafe_code)]

use crate::op_error::ToOpError;
use std::cell::RefCell;

use zeroship_runtime::state::{JsonValue, OpError};
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_async_method, v8_constructor, v8_method};

use crate::broker::{self, Subscription as BrokerSubscription, SubscriptionMessage};
use zeroship_data_orm::binding::DbRoute;

// ---------------------------------------------------------------------------
// CDC readiness - the adapter half
// ---------------------------------------------------------------------------

/// Resolve the adapter state a CDC start needs, then run the handshake.
///
/// The ORM owns process-wide readiness and teardown. The adapter supplies its
/// backend and the worker's authenticated relay configuration.
///
/// Keyed on the ROUTE: readiness is per `(app, database)` because one relay
/// connection carries one database's stream.
async fn ensure_cdc_ready(route: &DbRoute) -> Result<(), zeroship_data_orm::error::DbError> {
    let backend = crate::tx_scope::ensure_backend().await?;
    zeroship_data_orm::cdc::lifecycle::ensure_ready(route, backend, crate::tx_scope::cdc_relay())
        .await
}

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
    /// Process-wide CDC claim paired one-to-one with `inner`. Dropping the
    /// last claim shuts down the ORM relay client for this route.
    cdc_lease: RefCell<Option<zeroship_data_orm::cdc::lifecycle::CdcLease>>,
    route: DbRoute,
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
        self.cdc_lease.borrow_mut().take();
    }
}

// ---------------------------------------------------------------------------
// Subscription IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
impl Subscription {
    /// `new Subscription()` from JS rejects — real instances come from
    /// [`mint_subscription`] via `collection.openSubscription()` (the
    /// duplicate `db.openSubscription(name)` Db-level entry point was
    /// removed), which registers the broker entry as part
    /// of minting.
    #[v8_constructor]
    fn new() -> Result<Subscription, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// Refuse to advertise a healthy live stream until its change source is
    /// running. This is a first-class open handshake, not a background hint:
    /// configuration, provisioning, and START_REPLICATION errors reject it.
    #[v8_async_method]
    async fn ready(&self) -> Result<(), OpError> {
        if self.inner.borrow().is_none() {
            return Err(OpError::coded(
                "subscription_closed",
                "db subscription is closed",
                None::<String>,
            ));
        }
        ensure_cdc_ready(&self.route)
            .await
            .map_err(crate::op_error::ToOpError::to_op_error)
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
        // Snapshot the broker subscription handle so `.await` does not hold
        // a `RefCell` borrow. `BrokerSubscription` is a cheap Arc clone.
        let sub_opt: Option<BrokerSubscription> = self.inner.borrow().as_ref().cloned();
        let sub = match sub_opt {
            Some(s) => s,
            None => return Ok(JsonValue("null".into())),
        };

        // Direct native callers receive the same fail-loud contract as the
        // TypeScript wrapper even if they skip the explicit ready() call.
        ensure_cdc_ready(&self.route)
            .await
            .map_err(crate::op_error::ToOpError::to_op_error)?;

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
            self.cdc_lease.borrow_mut().take();
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
        self.cdc_lease.borrow_mut().take();
    }
}

// ---------------------------------------------------------------------------
// mint_subscription — build a wrapper from a fresh broker entry
// ---------------------------------------------------------------------------

/// Test-only failure-injection seam for the V8 allocation phase of
/// [`mint_subscription`]. Armed by
/// `tests::mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure`
/// to prove that a failure before the broker subscribe leaves no entry
/// behind. Never compiled into production builds.
#[cfg(test)]
static FAIL_MINT_ALLOC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Mint a `Subscription` JS wrapper for the given `(route, collection)` pair,
/// where the route is the tenant AND the database the binding addresses.
///
/// Allocates the JS object, looks up the class template and prototype,
/// and only THEN subscribes on the process-wide broker, so a `?`-
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
/// `crates/zeroship-runtime/src/rpc/ctx_holder.rs`.
pub fn mint_subscription<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    route: &DbRoute,
    collection: &str,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    // Test-only failure injection: stands in for the first fallible V8
    // operation failing. It sits ahead of every broker interaction, so an
    // armed flag must return without leaving a broker entry behind. See
    // `tests::mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure`.
    #[cfg(test)]
    if FAIL_MINT_ALLOC.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(OpError::type_error("simulated V8 allocation failure"));
    }

    let entries = crate::read_capture::current(scope)
        .map_err(ToOpError::to_op_error)?
        .snapshot_for(collection);
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

    // STEP 2 - broker subscribe through the production fallible gate. A
    // refusal owns no broker handle, so returning its typed OpError here
    // cannot leak an entry. From the success arm onward the broker entry's
    // ONLY owner is the `Subscription` state we're about to install in
    // internal field 0. The remaining operations (`Box::new`,
    // `Box::into_raw`, `External::new`, `set_internal_field`,
    // `with_guaranteed_finalizer`) are infallible: V8 `External::new`
    // and `set_internal_field` cannot fail (the slot exists per
    // `Subscription::install`), and `with_guaranteed_finalizer` is
    // documented as infallible. So no `?` can run between here and the
    // wrapper being live.
    let broker_sub = match broker::try_subscribe(route, collection) {
        Ok(subscription) => subscription,
        Err(error) => return Err(error.to_op_error()),
    };

    // Narrow delivery to the rows this handler actually read.
    //
    // `db.live(fn)` runs `fn` - which does the reads - and only then calls
    // `subscribe(name)` for each collection the tracker saw, so the buffer is
    // already populated by the time we get here. `snapshot_for` clones rather
    // than drains because that loop opens one subscription per collection.
    //
    // ATTACH ONLY A NON-EMPTY SET. `Subscription::accepts` treats `None` as
    // "coarse - take everything" but `Some(vec![])` as "take nothing" (see
    // `broker::tests::b8b_empty_read_set_filters_everything_on_collection`).
    // A `query()` that subscribes without having read this collection - or a
    // `mutation`/`action`, where capture is inert by design - would otherwise
    // attach an empty set and go permanently silent, which is strictly worse
    // than the coarse delivery it replaces.
    if !entries.is_empty() {
        broker_sub.set_read_set(entries);
    }

    let cdc_lease = zeroship_data_orm::cdc::lifecycle::acquire(route);

    let state = Subscription {
        inner: RefCell::new(Some(broker_sub)),
        cdc_lease: RefCell::new(Some(cdc_lease)),
        route: route.clone(),
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
    //! 2026-05-22, M-NEW-2): if `broker::try_subscribe` runs BEFORE the
    //! fallible V8 alloc chain, an error between the subscribe call and
    //! the wrapper install leaks a broker entry — `Broker::subscription_count`
    //! only prunes `is_closed()` slots, and `is_closed` is flipped
    //! exclusively by the JS-side wrapper's `Drop`/finalizer, which never
    //! runs if the wrapper was never constructed.
    //!
    //! `mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure`
    //! exercises that path behaviorally: it arms the test-only
    //! [`super::FAIL_MINT_ALLOC`] seam so the first fallible V8 op fails,
    //! then asserts the broker saw no entry. Moving `broker::try_subscribe`
    //! ahead of the seam makes the armed call subscribe first, so the
    //! count assertion fails.
    //!
    //! The integration test
    //! `tests/subscription_finalizer.rs::dropping_subscription_closes_broker_handle_on_gc`
    //! covers the happy path (V8 alloc succeeds → broker entry reclaimed
    //! on GC); this test covers the unhappy path.

    use std::collections::HashMap;

    use zeroship_data_orm::value::Value;
    use zeroship_runtime::{Runtime, RuntimeState, SharedState};

    use crate::{broker, v8_classes::db::mint_db};

    #[test]
    fn mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure() {
        // Behavioral guard: arm the test-only seam so `mint_subscription`
        // returns from its V8 alloc phase before it reaches
        // `broker::try_subscribe`, then assert the broker saw no entry.
        // If a refactor moves `broker::try_subscribe` ahead of the seam,
        // the armed call subscribes first and the count assertion fails.
        let handle = std::thread::spawn(|| {
            const APP_ID: &str = "test_app_alloc_fail";
            let route = crate::tests::fixtures::harness_route(APP_ID);
            assert_eq!(broker::app_subscription_count(APP_ID), 0);
            zeroship_runtime::init_v8();
            let mut isolate = v8::Isolate::new(v8::CreateParams::default());
            v8::scope!(let handle_scope, &mut isolate);
            let context = v8::Context::new(handle_scope, Default::default());
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            {
                v8::scope!(let inner, scope);
                super::FAIL_MINT_ALLOC.store(true, std::sync::atomic::Ordering::SeqCst);
                let result = super::mint_subscription(inner, &route, "messages");
                super::FAIL_MINT_ALLOC.store(false, std::sync::atomic::Ordering::SeqCst);
                assert!(
                    result.is_err(),
                    "armed failure seam must make mint_subscription return Err"
                );
                assert_eq!(
                    broker::app_subscription_count(APP_ID),
                    0,
                    "a V8 allocation failure must not leave a broker entry behind"
                );
            }
        });
        handle.join().expect("thread panicked");
    }

    #[test]
    fn mint_subscription_happy_path_registers_exactly_one_broker_entry() {
        // Sanity counterpart: when V8 alloc succeeds the function
        // DOES register a broker entry (so the failure-injection test
        // above isn't accidentally passing by removing the subscribe call
        // altogether). Run in a fresh thread so we don't race with
        // other broker users on this test runner thread.
        let handle = std::thread::spawn(|| {
            let route = crate::tests::fixtures::harness_route("test_app_unit");
            assert_eq!(broker::app_subscription_count("test_app_unit"), 0);
            zeroship_runtime::init_v8();
            let mut isolate = v8::Isolate::new(v8::CreateParams::default());
            v8::scope!(let handle_scope, &mut isolate);
            let context = v8::Context::new(handle_scope, Default::default());
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            {
                v8::scope!(let inner, scope);
                let _obj = super::mint_subscription(inner, &route, "messages")
                    .expect("mint_subscription should succeed");
                assert_eq!(
                    broker::app_subscription_count("test_app_unit"),
                    1,
                    "expected exactly one broker entry after mint"
                );
            }
            // Force GC so the finalizer reclaims the broker entry —
            // otherwise the process-wide broker carries an entry into
            // the (short) thread teardown and the cleanup-assertion
            // below would race the finalizer.
            scope.request_garbage_collection_for_testing(v8::GarbageCollectionType::Full);
            scope.perform_microtask_checkpoint();
            assert_eq!(broker::app_subscription_count("test_app_unit"), 0);
        });
        handle.join().expect("thread panicked");
    }

    #[test]
    fn open_subscription_refuses_the_257th_live_handle_with_typed_error() {
        const COLLECTION: &str = "users";
        let app_id = format!("subscription-cap-{}", uuid::Uuid::new_v4());
        let mut env_vars = HashMap::new();
        env_vars.insert("APP_ID".to_string(), app_id.clone());
        // A bare Runtime registers no plugin, so the host binding store has to
        // be installed directly for `mint_db` to reach one.
        crate::tests::fixtures::supply_app_bindings([app_id.as_str()]);
        let runtime = Runtime::builder().env_vars(env_vars).build();

        let outcome = runtime.with_scope(|scope| {
            let state: SharedState = std::rc::Rc::new(std::cell::RefCell::new(RuntimeState::new(
                HashMap::new(),
                None,
                None,
            )));
            scope.set_slot(state);

            let db = mint_db(scope, &app_id).expect("mint production Db binding");
            let global = scope.get_current_context().global(scope);
            let db_key = v8::String::new(scope, "db").unwrap();
            assert_eq!(
                global.set(scope, db_key.into(), db.into()),
                Some(true),
                "bind Db for creator script"
            );

            let source = format!(
                r#"(() => {{
                    const held = [];
                    let error = null;
                    try {{
                        for (let i = 0; i <= {limit}; i += 1) {{
                            held.push(db.collection({collection:?}).openSubscription());
                        }}
                    }} catch (caught) {{
                        error = caught;
                    }} finally {{
                        for (const subscription of held) subscription.close();
                    }}
                    return JSON.stringify({{
                        opened: held.length,
                        isError: error instanceof Error,
                        code: error?.code ?? null,
                        message: error?.message ?? null,
                        hint: error?.hint ?? null,
                    }});
                }})()"#,
                limit = broker::MAX_SUBSCRIPTIONS_PER_APP,
                collection = COLLECTION,
            );
            let source = v8::String::new(scope, &source).unwrap();
            let script = v8::Script::compile(scope, source, None).expect("compile creator script");
            let result = script
                .run(scope)
                .expect("creator script must catch the refusal");
            result.to_rust_string_lossy(scope)
        });

        let outcome: Value = serde_json::from_str(&outcome).expect("subscription outcome JSON");
        let opened = outcome["opened"]
            .as_u64()
            .expect("opened must be an integer");
        assert!(
            opened > 0,
            "production-path loop must open at least one subscription"
        );
        assert_eq!(
            opened,
            broker::MAX_SUBSCRIPTIONS_PER_APP as u64,
            "the production openSubscription path must refuse the first handle beyond the cap: {outcome}"
        );
        assert_eq!(
            outcome["isError"], true,
            "refusal must be a real JS Error: {outcome}"
        );
        assert_eq!(
            outcome["code"], "subscription_limit",
            "refusal must expose the typed creator-facing code: {outcome}"
        );
        assert!(
            outcome["message"]
                .as_str()
                .is_some_and(|message| message.contains("maximum of 256 concurrent subscriptions")),
            "refusal message must state the cap: {outcome}"
        );
        assert!(
            outcome["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("close unused subscriptions")),
            "refusal hint must tell the creator how to recover: {outcome}"
        );

        broker::drop_app(&app_id);
        assert_eq!(
            broker::app_subscription_count(&app_id),
            0,
            "test cleanup must remove every routing entry"
        );
    }
}
