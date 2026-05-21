//! V8 callbacks for `zeroship.db.*` methods.
//!
//! This file is the **registration shim** that re-exports the public
//! surface of the plugin's V8 dispatch helpers from their per-concern
//! homes:
//!
//! - V8 promise plumbing / value walker / row decoder → [`crate::v8_bridge`]
//! - SQL execution helpers (`run_sql`, `exec_query`, `exec_mutation_*`,
//!   `ensure_pool`, broker queue/drain) → [`crate::exec`]
//! - CRUD dispatch helpers (`dispatch_*`) → [`crate::crud`]
//! - DDL orchestrator (`exec_register_model_with_pool`) → (in-file below;
//!   will move to `crate::orchestrator::register_model`)
//! - Transaction lifecycle (begin/commit/rollback) → (in-file below;
//!   will move to `crate::orchestrator::transaction`)
//! - Auto-tx wrappers (`__zsBeginAutoTx` / `__zsEndAutoTx`) → (in-file
//!   below; will move to `crate::orchestrator::auto_tx`)
//! - Replication op dispatchers + auto-spawn → (in-file below; will move
//!   to `crate::replication_ops`)
//!
//! Re-exports here preserve the `callbacks::*` public path that
//! `v8_classes/*.rs` and `tests/integration.rs` import.

#[allow(unused_imports)]
use std::rc::Rc;

#[allow(unused_imports)]
use zeroship_runtime::state::{OpError, OpResult, ResolveValue, SharedState};
#[allow(unused_imports)]
use serde_json::Value;

#[allow(unused_imports)]
use crate::query;
#[allow(unused_imports)]
use crate::DB_POOL;

// Re-exports — preserve the legacy `callbacks::*` path while the
// concerns live in dedicated modules.
#[allow(unused_imports)]
pub(crate) use crate::v8_bridge::{
    get_string_arg, get_i64_arg, read_json_arg, refuse_if_query_capability,
    runtime_state, setup_js_promise, setup_promise, v8_value_to_serde_json,
};
#[allow(unused_imports)]
pub(crate) use crate::v8_bridge::{fmt_db_err, row_to_json, rows_to_json};
#[allow(unused_imports)]
pub(crate) use crate::v8_bridge::get_app_id_pub;
#[allow(unused_imports)]
pub(crate) use crate::exec::{
    clear_pending_emits, drain_pending_emits_on_commit, ensure_pool, exec_count,
    exec_mutation_with_emit, exec_query, run_sql,
};
pub use crate::exec::exec_mutation_with_emit_for_tests;

// CRUD dispatch helpers — see [`crate::crud`] for the implementations.
#[allow(unused_imports)]
pub(crate) use crate::crud::{
    dispatch_aggregate, dispatch_count, dispatch_delete_many, dispatch_delete_one,
    dispatch_distinct, dispatch_find, dispatch_find_one, dispatch_find_or_create,
    dispatch_insert, dispatch_insert_many, dispatch_update_many, dispatch_update_one,
    dispatch_upsert,
};

// Orchestrator entry points — see [`crate::orchestrator`].
pub use crate::orchestrator::register_model::{
    exec_register_model_with_pool, register_model_dispatch,
};


// ---------------------------------------------------------------------------
// Transaction callbacks: begin / commit / rollback
// ---------------------------------------------------------------------------
//
// V8 is single-threaded per isolate, so only one transaction can be active
// at a time. We store the transaction connection in TX_CONN thread-local.
// All CRUD callbacks (exec_query, exec_mutation, etc.) automatically use
// TX_CONN when it's set, via the run_sql() helper.

/// `zeroship.db.beginTransaction(isolationLevel?)` → Promise<Transaction>
///
/// Opens a dedicated connection, runs BEGIN, stores it in TX_CONN, and
/// resolves with a fresh [`crate::v8_classes::transaction::Transaction`]
/// v8_class instance. The wrapper's Weak finalizer auto-rollbacks if
/// the handle is dropped without `.commit()` / `.rollback()` — closes
/// the connection-leak footgun the pre-wrapper API had.
///
/// All subsequent CRUD ops use the transaction connection until
/// the wrapper's `.commit()` or `.rollback()` runs (or the wrapper's
/// `Drop` finalizer auto-rollbacks on GC).
///
/// The Transaction wrapper is minted *synchronously* before the BEGIN
/// future runs (we need a V8 scope to allocate it). On BEGIN success
/// the future stamps the wrapper's pre-allocated `token` onto
/// [`crate::TX_TOKEN`] and resolves the promise with the wrapper; on
/// failure the wrapper is left with a token that never matches
/// TX_TOKEN, so its `Drop` is a no-op when V8 eventually collects it.
pub fn begin_transaction_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    isolation_level: Option<String>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);

    // Allocate the ownership token + mint the wrapper synchronously.
    // We need a scope to allocate the V8 object; the spawned future
    // doesn't have one. On BEGIN success the future stamps TX_TOKEN
    // with this same token; on failure TX_TOKEN stays 0 so the
    // wrapper's Drop sees `current(0) != token` and no-ops.
    let token = crate::next_tx_token();
    let tx_obj = match crate::v8_classes::transaction::mint_transaction(scope, token, app_id) {
        Ok(obj) => obj,
        Err(e) => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let msg = v8::String::new(scope, &e.message).unwrap();
            let exc = v8::Exception::error(scope, msg);
            resolver.reject(scope, exc);
            return promise;
        }
    };

    let tx_obj_as_value: v8::Local<v8::Value> = tx_obj.into();
    let tx_global: v8::Global<v8::Value> = v8::Global::new(scope, tx_obj_as_value);

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_global = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_begin(isolation_level.as_deref()).await {
            Ok(()) => {
                // Stamp ownership now that TX_CONN holds the client —
                // the wrapper's commit / rollback / Drop all gate on
                // this matching the wrapper's `token`.
                crate::TX_TOKEN.with(|t| t.set(token));
                OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::JsGlobal(tx_global),
                    request_id,
                }
            }
            Err(e) => {
                // Wrapper's `token` never matches TX_TOKEN(=0), so when
                // V8 collects the wrapper (no JS reference survives a
                // rejected await) the Drop is a no-op.
                drop(tx_global);
                OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::RejectError(OpError::error(e)),
                    request_id,
                }
            }
        }
    }));

    promise
}

/// Allowed isolation levels (uppercased for validation).
const VALID_ISOLATION_LEVELS: &[&str] = &[
    "READ UNCOMMITTED",
    "READ COMMITTED",
    "REPEATABLE READ",
    "SERIALIZABLE",
];

async fn exec_begin(isolation_level: Option<&str>) -> Result<(), String> {
    // Check: no nested transactions
    let has_tx = crate::TX_CONN.with(|tx| tx.borrow().is_some());
    if has_tx {
        return Err("db: transaction already active (nested transactions not supported)".to_string());
    }

    // Build BEGIN statement with optional isolation level
    let begin_sql = match isolation_level {
        Some(level) => {
            let upper = level.to_uppercase();
            if !VALID_ISOLATION_LEVELS.contains(&upper.as_str()) {
                return Err(format!(
                    "db: invalid isolation level: {level}. Must be one of: read uncommitted, read committed, repeatable read, serializable"
                ));
            }
            format!("BEGIN ISOLATION LEVEL {upper}")
        }
        None => "BEGIN".to_string(),
    };

    // Open a dedicated connection (not from pool — we need to hold it).
    // compio-postgres splits a connection into (Client, Connection); we spawn
    // the Connection on a detached task so its run loop drives I/O, and store
    // the Client in TX_CONN. When the Client is eventually dropped, the task
    // terminates gracefully.
    let url = crate::DB_URL.with(|u| u.borrow().clone())
        .ok_or_else(|| "db: not configured".to_string())?;
    let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .map_err(|e| format!("db: tx connect failed: {e}"))?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("db: tx connection task error: {e}");
        }
    })
    .detach();

    client.execute(&begin_sql, &[])
        .await
        .map_err(|e| format!("db: BEGIN failed: {e}"))?;

    crate::TX_CONN.with(|tx| { tx.borrow_mut().replace(client); });
    // Defensive: any residue from a prior tx that didn't drain cleanly
    // (shouldn't happen — every settle path clears) must NOT leak into
    // the new tx's drain. Drop without firing.
    clear_pending_emits();
    Ok(())
}




// ---------------------------------------------------------------------------
// Auto-tx wrappers — defense-in-depth around query() / mutation() handlers
// ---------------------------------------------------------------------------
//
// `__zsBeginAutoTx(kindStr): Promise<number>`
//   Resolves with a numeric token the JS shim hands back to
//   `__zsEndAutoTx(token, success)`.
//
//     token = 0  → no auto-tx opened (kind is not "query"/"mutation", or a
//                  user-driven `db.transaction(...)` is already active, or
//                  the DB plugin is configured with a never-dialed dummy
//                  URL — capability gate fired before we got here).
//     token = 1  → auto-tx opened successfully; commit/rollback owed.
//
// `__zsEndAutoTx(token, success): Promise<void>`
//   Token 0 → resolved promise, no-op. Token 1 → COMMIT on success,
//   ROLLBACK on failure. Errors during commit/rollback are surfaced
//   verbatim to JS; the SSR shim still re-throws the underlying handler
//   error so callers don't see commit failures mask handler errors.
//
// Picks isolation level by kind:
//   query    → BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY
//   mutation → BEGIN ISOLATION LEVEL <override or READ COMMITTED> READ WRITE
//
// The mutation default is READ COMMITTED — same as Postgres's default
// for explicit BEGIN. Apps that need write-skew protection bump to
// `serializable` per-mutation via the wrapper config; apps that need
// consistent re-reads inside the handler bump to `repeatable read`.
// Stronger isolation costs throughput (SSI bookkeeping, more 40001
// retries) and is opt-in by design.
//
// `action`, `stream`, `subscription` and unknown kinds are not wrapped:
//   actions can hold open external IO, streams/subscriptions are long-
//   lived; both would starve the connection pool. The capability gate
//   (B3 runtime layer) is the primary enforcement; auto-tx is a second
//   line of defense at the Postgres level.

/// `globalThis.__zsBeginAutoTx(kindStr): Promise<number>` — see module
/// comment above.
pub fn auto_begin_transaction(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let kind = get_string_arg(scope, &args, 0);
    // Optional second arg: per-mutation isolation override
    // ("read committed" | "repeatable read" | "serializable"). Empty /
    // missing → use the per-kind default in `auto_tx_begin_sql`.
    let isolation = get_string_arg(scope, &args, 1);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_auto_begin(kind.as_deref(), isolation.as_deref()).await {
            Ok(token) => OpResult::Completed {
                op_id,
                value: token.to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

/// `globalThis.__zsEndAutoTx(token, success): Promise<void>` — see
/// module comment above.
pub fn auto_end_transaction(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let token = get_i64_arg(scope, &args, 0).unwrap_or(0);
    // Second arg is a boolean. `is_true()` covers the literal `true`;
    // we deliberately do NOT treat truthy non-booleans as success — the
    // SSR shim always passes a real boolean and any other shape is a
    // bug we want surfaced as a rollback (defense in depth).
    let success = args.length() >= 2 && args.get(1).is_true();
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_auto_end(token, success).await {
            Ok(()) => OpResult::Completed { op_id, value: "null".to_string(), request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

/// Per-kind BEGIN SQL. Returns `None` for kinds we don't wrap. For
/// mutations, `isolation` overrides the default READ COMMITTED.
fn auto_tx_begin_sql(kind: Option<&str>, isolation: Option<&str>) -> Option<String> {
    match kind {
        Some("query") => {
            Some("BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY".to_string())
        }
        Some("mutation") => {
            let level = normalize_isolation(isolation).unwrap_or("READ COMMITTED");
            Some(format!("BEGIN ISOLATION LEVEL {level} READ WRITE"))
        }
        _ => None,
    }
}

/// Case-fold + whitespace-collapse a user-supplied isolation string to
/// one of Postgres' four accepted values. Unknown / empty → `None`
/// (caller falls back to the default).
fn normalize_isolation(s: Option<&str>) -> Option<&'static str> {
    let raw = s?.trim();
    if raw.is_empty() {
        return None;
    }
    let upper: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase();
    match upper.as_str() {
        "READ COMMITTED" => Some("READ COMMITTED"),
        "REPEATABLE READ" => Some("REPEATABLE READ"),
        "SERIALIZABLE" => Some("SERIALIZABLE"),
        // READ UNCOMMITTED is accepted by Postgres but silently upgrades
        // to READ COMMITTED — treat as alias.
        "READ UNCOMMITTED" => Some("READ COMMITTED"),
        _ => None,
    }
}

async fn exec_auto_begin(
    kind: Option<&str>,
    isolation: Option<&str>,
) -> Result<u32, String> {
    // Skip if this kind isn't wrapped (action / stream / subscription /
    // unknown). Token 0 → end is a no-op.
    let Some(sql) = auto_tx_begin_sql(kind, isolation) else {
        return Ok(0);
    };

    // Don't wrap when a user-driven `db.transaction(...)` already
    // holds TX_CONN — that would be a nested-tx attempt the user
    // already opted out of. Returning 0 keeps the auto-end callback
    // off the user's tx entirely.
    let has_tx = crate::TX_CONN.with(|tx| tx.borrow().is_some());
    if has_tx {
        return Ok(0);
    }

    // Open a dedicated connection (same pattern as user-driven
    // `begin_transaction`). compio-postgres splits the connection into
    // (Client, Connection); spawn the run loop on a detached task, hold
    // the Client in TX_CONN.
    let url = crate::DB_URL.with(|u| u.borrow().clone())
        .ok_or_else(|| "db: not configured".to_string())?;
    let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .map_err(|e| format!("db: auto-tx connect failed: {e}"))?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("db: auto-tx connection task error: {e}");
        }
    })
    .detach();

    client.execute(&sql, &[])
        .await
        .map_err(|e| format!("db: auto-tx BEGIN failed: {e}"))?;

    crate::TX_CONN.with(|tx| { tx.borrow_mut().replace(client); });
    crate::AUTO_TX_OWNED.with(|f| f.set(true));
    clear_pending_emits();
    Ok(1)
}

async fn exec_auto_end(token: i64, success: bool) -> Result<(), String> {
    // No-op tokens (unwrapped kinds) — nothing to commit.
    if token == 0 {
        return Ok(());
    }

    // Defensive ownership check: if AUTO_TX_OWNED is false the tx is
    // either gone or owned by user code. Either way leave it alone.
    let owned = crate::AUTO_TX_OWNED.with(|f| f.get());
    if !owned {
        return Ok(());
    }

    // Take the client. We MUST clear AUTO_TX_OWNED before any await so
    // a re-entrant call (shouldn't happen — V8 is single-threaded —
    // but cheap insurance) doesn't see stale ownership.
    let client = crate::TX_CONN.with(|tx| tx.borrow_mut().take());
    crate::AUTO_TX_OWNED.with(|f| f.set(false));

    let Some(client) = client else {
        // Ownership flag said yes, conn slot is empty: state is
        // inconsistent. Treat as already-ended.
        return Ok(());
    };

    let cmd = if success { "COMMIT" } else { "ROLLBACK" };
    let result = client.execute(cmd, &[]).await;
    drop(client); // explicit — terminates the spawned connection task.

    // Settle the deferred broker queue. On a successful COMMIT, fire
    // every event we'd have published mid-tx; on ROLLBACK (or COMMIT
    // failure) drop them silently so subscribers never see writes
    // Postgres just undid.
    if success && result.is_ok() {
        drain_pending_emits_on_commit();
    } else {
        clear_pending_emits();
    }

    result
        .map(|_| ())
        .map_err(|e| {
            // Walk the source chain so deferred-FK / unique violations
            // surface with their SQLSTATE detail. compio-postgres' Display
            // for Error::Db writes only "db error"; the real message
            // (`ERROR: insert or update on table "todos" violates
            // foreign key constraint ...`) lives on the cause.
            let mut msg = format!("db: auto-tx {cmd} failed: {e}");
            let mut cur: &dyn std::error::Error = &e;
            while let Some(src) = std::error::Error::source(cur) {
                msg.push_str(&format!(" — caused by: {src}"));
                cur = src;
            }
            msg
        })
}

/// Install `__zsBeginAutoTx` / `__zsEndAutoTx` on `globalThis`. Called
/// once during plugin `register()` via [`NativeRegistrar::add_setup`].
pub fn install_auto_tx_globals(scope: &mut v8::PinScope<'_, '_>) {
    let global = scope.get_current_context().global(scope);
    {
        let f = v8::Function::new(scope, auto_begin_transaction).unwrap();
        let key = v8::String::new(scope, "__zsBeginAutoTx").unwrap();
        global.set(scope, key.into(), f.into());
    }
    {
        let f = v8::Function::new(scope, auto_end_transaction).unwrap();
        let key = v8::String::new(scope, "__zsEndAutoTx").unwrap();
        global.set(scope, key.into(), f.into());
    }
}

// ===========================================================================
// B1 — @zeroship/migrations primitives
//
// These callbacks are the V8 bridge for the migrations module
// (`crate::migrations`). Each callback parses its arguments, sets up
// a promise, and spawns an async op that delegates to the matching
// `exec_*` function. See `migrations.rs` for the semantics.
// ===========================================================================


// ===========================================================================
// C1 (P8a) — reactive queries via the in-process subscription broker
//
// JS surface (defined in sdks/db/src/subscribe.ts):
//
//   const sub = env.db.openSubscription(collection)   // → Subscription wrapper
//   const msg = await sub.pollJson()                  // → JSON event | null
//   sub.close()                                       // synchronous, idempotent
//   await env.db.replicationSetup()                   // → JSON setup outcome
//   await env.db.replicationWatchdog()                // → JSON [{slot,...}]
//   await env.db.replicationDropAbandoned(seconds)    // → JSON [dropped slot names]
//
// The Subscription wrapper is a `#[v8_class]` instance — its Weak
// finalizer closes the broker entry on GC, so callers that drop the
// wrapper without `.close()` still release the slot.
// ===========================================================================

/// `zeroship.db.openSubscription(collection)` → `Subscription` wrapper
///
/// Synchronous: subscribes on the thread-local broker, mints a
/// `Subscription` v8_class instance, returns it directly. The
/// wrapper's Weak finalizer closes the broker entry on GC, so callers
/// that drop the JS reference without `.close()` still release the
/// slot. Use `.pollJson()` to drain events (returns
/// `Promise<string|null>`) and `.close()` for idempotent explicit
/// teardown.
/// `db.replication.setup(opts?)` dispatch — see [`Db::replication`].
pub fn replication_setup_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
            Ok(out) => OpResult::Completed {
                op_id,
                value: out.to_json(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    promise
}

/// `db.replication.watchdog()` dispatch — see [`Db::replication`].
pub fn replication_watchdog_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::replication::watchdog_query(&pool).await {
            Ok(rows) => OpResult::Completed {
                op_id,
                value: crate::replication::watchdog_to_json(&rows),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    promise
}

/// `db.replication.dropAbandoned(opts?)` dispatch — see
/// [`Db::replication`].
pub fn replication_drop_abandoned_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    inactive_seconds: i64,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::replication::drop_abandoned_slots(&pool, inactive_seconds).await {
            Ok(names) => OpResult::Completed {
                op_id,
                value: serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    promise
}

// ---------------------------------------------------------------------------
// Auto-spawn — `zeroship.db.startReplicationConsumer()`
// ---------------------------------------------------------------------------
//
// Apps that opt into reactive queries call this once at module init
// (`await env.db.startReplicationConsumer()`). It:
//   1. Provisions the publication + slot (idempotent — same as
//      `replicationSetup`).
//   2. Spawns a supervised WAL consumer task on the isolate's compio
//      runtime. The task lives for the isolate's lifetime and reconnects
//      with exponential backoff on transient failure (see
//      [`crate::wal_consumer::run_supervised`]).
//   3. Returns the `SetupOutcome` JSON.
//
// Idempotent — second call returns a JSON envelope with
// `{"alreadyRunning": true}` and short-circuits without spawning a
// second task. Tracked per-thread because the consumer task is
// thread-bound (the compio runtime is one per worker, the broker is
// thread-local).
//
// We chose explicit opt-in (a) over implicit spawn-on-first-subscribe
// (b): the failure surfaces at the call site, not deep inside a
// subscribe Promise. Apps with no reactive surface skip the cost.

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
        let already = RUNNING_CONSUMERS
            .with(|r| r.borrow().contains(&app_id));
        if already {
            let value = serde_json::json!({
                "alreadyRunning": true,
                "app_id": app_id,
            })
            .to_string();
            return OpResult::Completed { op_id, value, request_id };
        }

        // Step 1: provision (idempotent).
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        let setup = match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
            Ok(s) => s,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
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
            m.insert(
                "consumerStarted".into(),
                serde_json::Value::Bool(true),
            );
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
