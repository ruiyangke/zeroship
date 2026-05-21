//! V8 bridge for the `db.replication.*` operator namespace and the
//! `db.startReplicationConsumer()` auto-spawn entry point.
//!
//! The operator dispatchers ([`replication_setup_dispatch`],
//! [`replication_watchdog_dispatch`],
//! [`replication_drop_abandoned_dispatch`]) are thin wrappers that
//! grab the pool via [`crate::exec::ensure_pool`] and forward to the
//! pool-driven helpers in [`crate::replication`].
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
//! second task. Tracked per-thread via [`RUNNING_CONSUMERS`] because
//! the consumer task is thread-bound (the compio runtime is one per
//! worker, the broker is thread-local).
//!
//! We chose explicit opt-in (a) over implicit spawn-on-first-subscribe
//! (b): the failure surfaces at the call site, not deep inside a
//! subscribe Promise. Apps with no reactive surface skip the cost.

use zeroship_runtime::state::OpResult;

use crate::exec::ensure_pool;
use crate::v8_bridge::{runtime_state, setup_promise};

/// `db.replication.setup(opts?)` dispatch.
pub fn replication_setup_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                return OpResult::Failed {
                    op_id,
                    error: e.into_string(),
                    request_id,
                }
            }
        };
        match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
            Ok(out) => OpResult::Completed {
                op_id,
                value: out.to_json(),
                request_id,
            },
            Err(e) => OpResult::Failed {
                op_id,
                error: e,
                request_id,
            },
        }
    }));
    promise
}

/// `db.replication.watchdog()` dispatch.
pub fn replication_watchdog_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                return OpResult::Failed {
                    op_id,
                    error: e.into_string(),
                    request_id,
                }
            }
        };
        match crate::replication::watchdog_query(&pool).await {
            Ok(rows) => OpResult::Completed {
                op_id,
                value: crate::replication::watchdog_to_json(&rows),
                request_id,
            },
            Err(e) => OpResult::Failed {
                op_id,
                error: e,
                request_id,
            },
        }
    }));
    promise
}

/// `db.replication.dropAbandoned(opts?)` dispatch.
pub fn replication_drop_abandoned_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    inactive_seconds: i64,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                return OpResult::Failed {
                    op_id,
                    error: e.into_string(),
                    request_id,
                }
            }
        };
        match crate::replication::drop_abandoned_slots(&pool, inactive_seconds).await {
            Ok(names) => OpResult::Completed {
                op_id,
                value: serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()),
                request_id,
            },
            Err(e) => OpResult::Failed {
                op_id,
                error: e,
                request_id,
            },
        }
    }));
    promise
}

thread_local! {
    /// Per-thread "is the consumer already running for this app?"
    /// guard. Keyed by app_id (a single worker may host multiple apps
    /// over its lifetime via the LRU cache, but only one consumer per
    /// app at a time).
    static RUNNING_CONSUMERS: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// `zeroship.db.startReplicationConsumer()` → Promise<SetupOutcome JSON>
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
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Idempotent: if a consumer is already running for this app on
        // this thread, return a short-circuit envelope.
        let already = RUNNING_CONSUMERS.with(|r| r.borrow().contains(&app_id));
        if already {
            let value = serde_json::json!({
                "alreadyRunning": true,
                "app_id": app_id,
            })
            .to_string();
            return OpResult::Completed {
                op_id,
                value,
                request_id,
            };
        }

        // Step 1: provision (idempotent).
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                return OpResult::Failed {
                    op_id,
                    error: e.into_string(),
                    request_id,
                }
            }
        };
        let setup = match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
            Ok(s) => s,
            Err(e) => {
                return OpResult::Failed {
                    op_id,
                    error: e,
                    request_id,
                }
            }
        };

        // Step 2: build the consumer descriptor.
        let url = crate::DB_URL
            .with(|u| u.borrow().clone())
            .unwrap_or_default();
        let consumer = match crate::wal_consumer::WalConsumer::new(&app_id, &url) {
            Ok(c) => c.with_start_lsn(setup.confirmed_flush_lsn.clone()),
            Err(e) => {
                return OpResult::Failed {
                    op_id,
                    error: e.to_string(),
                    request_id,
                };
            }
        };

        // Step 3: spawn the supervised task. `detach()` lets it run
        // for the lifetime of the isolate's compio runtime — there's
        // nowhere to join it, and the supervisor exits cleanly on
        // CopyDone or a fatal error.
        //
        // Mark the app as running BEFORE the spawn so a racing second
        // call to startReplicationConsumer() short-circuits even if
        // the consumer task hasn't yet entered its decode loop.
        let app_for_task = app_id.clone();
        RUNNING_CONSUMERS.with(|r| {
            r.borrow_mut().insert(app_id.clone());
        });
        compio::runtime::spawn(async move {
            crate::wal_consumer::run_supervised(consumer).await;
            // When the supervisor exits (graceful or fatal), free the
            // slot so a later opt-in re-spawn is allowed.
            RUNNING_CONSUMERS.with(|r| {
                r.borrow_mut().remove(&app_for_task);
            });
        })
        .detach();

        // Resolve with the setup outcome plus the "running" marker.
        let mut env = serde_json::from_str::<serde_json::Value>(&setup.to_json())
            .unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(ref mut m) = env {
            m.insert("consumerStarted".into(), serde_json::Value::Bool(true));
            m.insert("alreadyRunning".into(), serde_json::Value::Bool(false));
        }
        OpResult::Completed {
            op_id,
            value: env.to_string(),
            request_id,
        }
    }));
    promise
}

/// **Test-only**: probe whether the auto-spawn registry holds an entry
/// for `app_id`. Used by `tests/integration.rs` to assert idempotency
/// without reaching into private state.
#[doc(hidden)]
pub fn is_consumer_registered_for_tests(app_id: &str) -> bool {
    RUNNING_CONSUMERS.with(|r| r.borrow().contains(app_id))
}

/// **Test-only**: clear the auto-spawn registry. Used to reset state
/// between integration tests that share a thread.
#[doc(hidden)]
pub fn clear_consumer_registry_for_tests() {
    RUNNING_CONSUMERS.with(|r| r.borrow_mut().clear());
}
