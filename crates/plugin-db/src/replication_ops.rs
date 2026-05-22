//! V8 bridge for the `db.replication.*` operator namespace and the
//! `db.startReplicationConsumer()` auto-spawn entry point.
//!
//! The operator dispatchers ([`replication_setup_dispatch`],
//! [`replication_watchdog_dispatch`],
//! [`replication_drop_abandoned_dispatch`]) are thin wrappers that
//! grab the pool via `ensure_pool` and forward to the pool-driven
//! helpers in [`crate::replication`].
//!
//! [`start_replication_consumer_dispatch`] is the opt-in entry point
//! apps call from module init to wire up reactive queries. It:
//!
//! 1. Provisions the publication + slot (idempotent — same as
//!    `replicationSetup`).
//! 2. Spawns a supervised WAL consumer task on the isolate's compio
//!    runtime. The task lives for the isolate's lifetime and
//!    reconnects with exponential backoff on transient failure (see
//!    [`crate::wal_consumer::run_supervised`]).
//! 3. Returns the `SetupOutcome` JSON.
//!
//! Idempotent — second call returns a JSON envelope with
//! `{"alreadyRunning": true}` and short-circuits without spawning a
//! second task. Tracked per-thread via the per-isolate context's
//! `running_consumers` slot (`IsolateDbContext::running_consumers`)
//! because the consumer task is thread-bound (the compio runtime is
//! one per worker, the broker is thread-local).
//!
//! We chose explicit opt-in (a) over implicit spawn-on-first-subscribe
//! (b): the failure surfaces at the call site, not deep inside a
//! subscribe Promise. Apps with no reactive surface skip the cost.
//!
//! ## Error rail
//!
//! Every dispatch failure here is routed through [`crate::error::DbError`]
//! → [`crate::error::DbError::to_op_error`] so the rejection carries a
//! stable `.code` the SDK can branch on:
//!
//! - `ensure_pool` failures preserve their DbError variant verbatim
//!   (typically `not_configured` / `transient`).
//! - The `replication::*` helpers return `Result<_, DbError>` directly
//!   — SQLSTATE classification + Configuration/Transient/LockContention
//!   variants are picked inside `crate::replication` and flow through
//!   here verbatim (no Internal-wrapping at the dispatch boundary).
//! - `WalConsumer::new` failures map to [`crate::error::DbError::Configuration`]
//!   with `code = "not_provisioned"` (the only thing that can fail
//!   pre-spawn is sanitisation of `app_id`).

use zeroship_runtime::state::{OpResult, ResolveValue};

use crate::error::DbError;
use crate::exec::ensure_pool;
use crate::v8_bridge::{runtime_state, setup_js_promise};

/// `db.replication.setup(opts?)` dispatch.
pub fn replication_setup_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                }
            }
        };
        match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
            Ok(out) => OpResult::JsValue {
                resolver,
                value: ResolveValue::String(out.to_json()),
                request_id,
            },
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));
    promise
}

/// `db.replication.watchdog()` dispatch.
///
/// `app_id` is the mint-time stamp from the `Replication` v8_class
/// wrapper — see the cross-tenant scoping note on
/// [`crate::replication::watchdog_query`]. The dispatch boundary never
/// reads an `appId` field from JS opts; callers in `v8_classes/replication.rs`
/// always pass `self.app_id`.
pub fn replication_watchdog_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                }
            }
        };
        match crate::replication::watchdog_query(&pool, &app_id).await {
            Ok(rows) => OpResult::JsValue {
                resolver,
                value: ResolveValue::String(crate::replication::watchdog_to_json(&rows)),
                request_id,
            },
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));
    promise
}

/// `db.replication.dropAbandoned(opts?)` dispatch.
///
/// `app_id` is the mint-time stamp from the `Replication` v8_class
/// wrapper — see the cross-tenant scoping note on
/// [`crate::replication::drop_abandoned_slots`]. The dispatch boundary
/// never reads an `appId` field from JS opts; callers in
/// `v8_classes/replication.rs` always pass `self.app_id`.
pub fn replication_drop_abandoned_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
    inactive_seconds: i64,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                }
            }
        };
        match crate::replication::drop_abandoned_slots(&pool, &app_id, inactive_seconds).await {
            Ok(names) => OpResult::JsValue {
                resolver,
                value: ResolveValue::String(
                    serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()),
                ),
                request_id,
            },
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));
    promise
}

/// `zeroship.db.startReplicationConsumer()` → `Promise<SetupOutcome JSON>`
///
/// Idempotent. The first call provisions the slot+publication, spawns
/// a supervised WAL consumer for the current app, and resolves once
/// `replicationSetup` returns (i.e. provisioning is durable). The
/// consumer continues running on the compio runtime in the background;
/// it suppresses local-emit for this app via the per-app suppression
/// gate so subscribers receive each event exactly once via WAL.
///
/// Subsequent calls short-circuit and resolve with the cached outcome
/// envelope plus `"alreadyRunning": true`.
pub fn start_replication_consumer_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Idempotent: if a consumer is already running for this app on
        // this thread, return a short-circuit envelope.
        let already = crate::context::with(|c| c.is_consumer_running(&app_id));
        if already {
            let value = serde_json::json!({
                "alreadyRunning": true,
                "app_id": app_id,
            })
            .to_string();
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::String(value),
                request_id,
            };
        }

        // Step 1: provision (idempotent).
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                }
            }
        };
        let setup = match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
            Ok(s) => s,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                }
            }
        };

        // Step 2: build the consumer descriptor.
        //
        // WalConsumer::new now returns Result<_, DbError> (post r5-r7
        // MAJOR-R5-4 fix). Two failure classes flow through verbatim:
        // - DbError::ValidationFailed { code: "invalid_app_id" } for
        //   developer/deploy errors (sanitise failure).
        // - DbError::Configuration { code: "not_provisioned" } for
        //   operator/configuration errors (missing db_url).
        // The dispatch boundary no longer re-stamps the error.
        let url = crate::context::with(|c| c.db_url()).unwrap_or_default();
        let consumer = match crate::wal_consumer::WalConsumer::new(&app_id, &url) {
            Ok(c) => c.with_start_lsn(setup.confirmed_flush_lsn.clone()),
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                };
            }
        };

        // Step 3: spawn the supervised task. `detach()` lets it run
        // for the lifetime of the isolate's compio runtime — there's
        // nowhere to join it, and the supervisor exits cleanly on
        // CopyDone or a fatal error.
        //
        // Mark + unmark live on a ConsumerRunningGuard whose lifetime
        // is bound to the spawned future. Both mark and unmark execute
        // INSIDE the future (mark on guard construction via try_claim;
        // unmark via Drop, on ANY exit — graceful, panic, future
        // dropped before first poll). The outer is_consumer_running
        // gate at line 195 short-circuits the common case (same-thread
        // re-call); try_claim closes the tight race window where two
        // rapid-succession dispatches both pass the outer gate before
        // either has marked.
        //
        // History (each pass closed a specific failure mode):
        // - e399eeea (cycle 05:25, MAJOR-R5-2): added Drop-based unmark
        //   so panic in run_supervised doesn't leave the mark stuck.
        // - 34d209b5 (cycle 06:00, MAJOR-R6-1): moved the mark INSIDE
        //   the guard's constructor (was previously synchronous before
        //   spawn) so future-dropped-pre-poll also fires Drop.
        // - 70921112 (cycle 06:25, concurrency r7 NEW MINOR): replaced
        //   bare mark with atomic try_mark so two concurrent dispatches
        //   that race past the outer gate cannot both spawn — the loser
        //   bails without provisioning, the winner runs run_supervised.
        //
        // Defense: spawned task try-marks atomically. If
        // another task won the race, the loser bails without
        // provisioning or marking. The winner constructs the guard,
        // ensuring Drop unmarks on every exit path.
        struct ConsumerRunningGuard {
            app_id: String,
        }
        impl ConsumerRunningGuard {
            /// Try to claim the running-marker; returns Some(guard) on
            /// success, None if another task already holds it.
            fn try_claim(app_id: String) -> Option<Self> {
                let won = crate::context::with_mut(|c| {
                    c.try_mark_consumer_running(&app_id)
                });
                won.then_some(Self { app_id })
            }
        }
        impl Drop for ConsumerRunningGuard {
            fn drop(&mut self) {
                crate::context::with_mut(|c| {
                    c.unmark_consumer_running(&self.app_id)
                });
            }
        }
        let app_for_task = app_id.clone();
        compio::runtime::spawn(async move {
            let Some(_guard) = ConsumerRunningGuard::try_claim(app_for_task) else {
                // Another task won the race; nothing to do.
                return;
            };
            crate::wal_consumer::run_supervised(consumer).await;
            // _guard drops here on graceful exit; Drop also fires on
            // panic-unwind, so the running marker is always cleared.
        })
        .detach();

        // Resolve with the setup outcome plus the "running" marker.
        let mut env = serde_json::from_str::<serde_json::Value>(&setup.to_json())
            .unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(ref mut m) = env {
            m.insert("consumerStarted".into(), serde_json::Value::Bool(true));
            m.insert("alreadyRunning".into(), serde_json::Value::Bool(false));
        }
        OpResult::JsValue {
            resolver,
            value: ResolveValue::String(env.to_string()),
            request_id,
        }
    }));
    promise
}

/// **Test-only**: probe whether the auto-spawn registry holds an entry
/// for `app_id`. Used by `tests/integration.rs` to assert idempotency
/// without reaching into private state.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn is_consumer_registered_for_tests(app_id: &str) -> bool {
    crate::context::with(|c| c.is_consumer_running(app_id))
}

/// **Test-only**: clear the auto-spawn registry. Used to reset state
/// between integration tests that share a thread.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn clear_consumer_registry_for_tests() {
    crate::context::with_mut(|c| c.clear_consumer_registry());
}
