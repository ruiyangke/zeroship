//! Native `Db.transaction(asyncFn, opts?)` orchestrator (P9 PR 3).
//!
//! Transaction orchestration lives **entirely in Rust** as of P9 PR 3.
//! The creator API is unchanged — `await env.db.transaction(async tx =>
//! {...})` commits on resolve, rolls back on throw — but the begin /
//! commit / rollback / nested-savepoint state machine moved out of the
//! bootstrap's JS `transactionImpl` and into [`transaction_dispatch`].
//! `db.beginTransaction()` and the `Transaction` v8_class methods
//! (`commit`/`rollback`/`collection`) no longer exist on the JS surface.
//!
//! ## The 8-step orchestrator (§4.4 of the P9 proposal)
//!
//! 1. [`transaction_dispatch`] (a sync v8_method body) mints the outer
//!    [`v8::PromiseResolver`] and returns its promise to JS immediately.
//! 2. It reads the per-isolate tx state ([`crate::context`]) to decide
//!    whether this is a **top-level** transaction (no tx active → emit
//!    `BEGIN`) or a **nested** one (a tx — auto-tx or explicit — already
//!    holds the `tx_conn` slot → emit `SAVEPOINT zs_sp_<N>`). Nesting
//!    beyond [`MAX_SAVEPOINT_DEPTH`] rejects with `savepoint_depth_exceeded`.
//! 3. A spawned op runs the `BEGIN` / `SAVEPOINT` SQL against the pinned
//!    connection. On success it hands back a
//!    [`zeroship_runtime::state::ResolveValue::Continuation`] (see step
//!    4); on failure it rejects the outer promise (`begin_failed` for a
//!    top-level BEGIN; the underlying coded error for a savepoint).
//! 4. The continuation runs inside the pump's V8 scope:
//!    [`mint_tx_view`](crate::v8_classes::transaction::mint_tx_view)
//!    builds the tx-view object (collections-as-props, no
//!    commit/rollback methods), then the creator callback is invoked
//!    inside a [`v8::TryCatch`] to capture a synchronous throw.
//! 5. The callback's return is coerced to a Promise
//!    ([`coerce_to_promise`]): an already-Promise is used as-is; a plain
//!    value is wrapped resolved; a synchronous throw skips straight to
//!    the rollback path.
//! 6. `.then(resolve_handler, reject_handler)` is attached to that
//!    Promise. The handlers are native [`v8::Function`]s whose `.data()`
//!    carries a heap [`TxFinalizer`] (the outer resolver + savepoint
//!    name + request id).
//! 7. On the creator promise **resolving**, [`tx_resolve_handler`] runs
//!    `COMMIT` (top-level) or `RELEASE SAVEPOINT zs_sp_<N>` (nested),
//!    then resolves the outer promise with the body result.
//! 8. On the creator promise **rejecting**, [`tx_reject_handler`] runs
//!    `ROLLBACK` (top-level) or `ROLLBACK TO SAVEPOINT zs_sp_<N>`
//!    (nested), then rejects the outer promise with the body error.
//!
//! ## Single-connection model & backend scope
//!
//! V8 is single-threaded per isolate, so only one transaction connection
//! is active at a time. It lives in [`crate::context::IsolateDbContext::tx_conn`];
//! every CRUD callback ([`crate::exec::run_sql`]) routes through that slot
//! when it is set. Nested savepoints reuse the **same** connection (that
//! is the whole point of `SAVEPOINT`), so no new connection is acquired
//! for a nested `transaction()`.
//!
//! Like the pre-P9 `begin_transaction` and the `auto_tx` wrapper, the
//! top-level `BEGIN` path acquires a dedicated Postgres client via
//! [`crate::backend::SqlExecutor::acquire_dedicated_client`] and is
//! therefore **Postgres-bound** today — the `tx_conn` slot itself holds a
//! `compio_postgres::Client`, and `run_sql` only consults it on the PG
//! path. SQLite routes CRUD through its session actor and does not
//! participate in `tx_conn`; a SQLite `transaction()` rejects with
//! `backend_unsupported`, matching the pre-existing `beginTransaction`
//! behaviour. (The savepoint SQL the orchestrator emits is plain-standard
//! and is exercised against the SQLite engine directly in
//! `tests/sqlite_integration.rs` so it is validated for the eventual
//! SQLite-tx wiring.)

#![allow(unsafe_code)]

use std::cell::Cell;

use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};

use crate::backend::SqlExecutor;
use crate::error::DbError;
use crate::exec::{clear_pending_emits, drain_pending_emits_on_commit};
use crate::v8_bridge::runtime_state;

/// Maximum nesting depth for `env.db.transaction(...)` calls — the
/// outermost `BEGIN` plus this many `SAVEPOINT` levels. A `transaction()`
/// call that would open the `(MAX_SAVEPOINT_DEPTH + 1)`-th savepoint
/// rejects with `savepoint_depth_exceeded`.
///
/// 8 matches the proposal's depth cap (Q-P9-D). Real code rarely nests
/// transactions beyond two or three levels; the cap is a runaway-recursion
/// guard, not a workload limit.
pub const MAX_SAVEPOINT_DEPTH: u32 = 8;

/// Allowed isolation levels (uppercased for validation).
const VALID_ISOLATION_LEVELS: &[&str] = &[
    "READ UNCOMMITTED",
    "READ COMMITTED",
    "REPEATABLE READ",
    "SERIALIZABLE",
];

// ---------------------------------------------------------------------------
// TxFinalizer — heap state shared by the resolve / reject handlers
// ---------------------------------------------------------------------------

/// Owned state the begin-continuation attaches (as a `v8::External`) to
/// the resolve and reject handler functions via `.data()`.
///
/// Both handlers point at the **same** boxed `TxFinalizer`. V8 settles a
/// promise exactly once and calls exactly one of the two `.then`
/// handlers, exactly once — so whichever handler fires takes ownership of
/// the box (`Box::from_raw`), reads what it needs, and drops it at the end
/// of the handler. The other handler's `External` dangles but is never
/// dereferenced. The `settled` cell is belt-and-braces against a
/// pathological double-call (it is read before the box is reclaimed).
struct TxFinalizer {
    /// The promise `Db.transaction(fn)` returned to JS. The settle path
    /// resolves it with the body result (commit) or rejects it with the
    /// body error (rollback).
    outer: v8::Global<v8::PromiseResolver>,
    /// Owning request id — routes the follow-up commit/rollback op's
    /// logs + cancellation to the request that opened the tx.
    request_id: Option<u64>,
    /// `None` ⇒ this `transaction()` opened the outermost `BEGIN`
    /// (settle = `COMMIT` / `ROLLBACK`, then drop the connection).
    /// `Some("zs_sp_N")` ⇒ this `transaction()` opened a nested
    /// `SAVEPOINT` (settle = `RELEASE SAVEPOINT zs_sp_N` /
    /// `ROLLBACK TO SAVEPOINT zs_sp_N`, connection stays open for the
    /// enclosing tx).
    savepoint: Option<String>,
    /// One-shot guard. Set the first time either handler fires.
    settled: Cell<bool>,
}

// ---------------------------------------------------------------------------
// transaction_dispatch — the v8_method entry point
// ---------------------------------------------------------------------------

/// `env.db.transaction(asyncFn, opts?)` → `Promise<R>`.
///
/// Synchronously mints the outer promise + decides BEGIN-vs-SAVEPOINT,
/// then spawns the begin/savepoint op. Returns the outer promise; the
/// creator callback runs once the begin op completes (see the module
/// comment for the full 8-step flow). `asyncFn` is validated to be a
/// function here; `opts.isolationLevel` is parsed + normalised by the
/// caller (`Db::transaction`) and arrives as an already-validated SQL
/// string.
pub fn transaction_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    user_fn: v8::Local<v8::Function>,
    isolation_level: Option<String>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);

    // Outer promise — returned to JS now; settled by the commit/rollback
    // handler after the creator callback runs.
    let outer_resolver = v8::PromiseResolver::new(scope).unwrap();
    let outer_promise = outer_resolver.get_promise(scope);
    let outer_global = v8::Global::new(scope, outer_resolver);
    let request_id = state.borrow().executing_request_id;

    // Capture the creator callback so the continuation can call it inside
    // the pump scope.
    let user_fn_global = v8::Global::new(scope, user_fn);

    // Decide BEGIN vs SAVEPOINT from the *current* tx state. `has_tx()`
    // is true whenever any tx holds the slot — auto-tx (query/mutation
    // wrapper) or an enclosing explicit `transaction()`. A nested call
    // therefore emits `SAVEPOINT` and reuses the open connection.
    let nested = crate::context::with(|c| c.has_tx());

    // Savepoint-depth cap: refuse the (MAX+1)-th level up front, before
    // any SQL runs. The depth that *would* be opened is the current
    // depth + 1.
    if nested {
        let would_be = crate::context::with(|c| c.savepoint_depth()) + 1;
        if would_be > MAX_SAVEPOINT_DEPTH {
            let err = DbError::validation_hinted(
                "savepoint_depth_exceeded",
                format!(
                    "db.transaction: nested transaction depth limit ({MAX_SAVEPOINT_DEPTH}) \
                     exceeded — flatten the nesting or split the work into separate transactions"
                ),
                "Each nested env.db.transaction(...) opens a SAVEPOINT; the cap guards against \
                 runaway recursion.",
            );
            // Resolve nothing yet exists to clean up (no SAVEPOINT was
            // emitted) — reject the outer promise directly.
            reject_outer_now(scope, &outer_global, err);
            return outer_promise;
        }
    }

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Drain note: the JS-side DataLoader queues are flushed by the
        // bootstrap wrapper *before* it calls this native method (those
        // microtask queues are pure-JS state with no Rust counterpart).
        // The Rust-side broker `pending_emits` queue is cleared on every
        // top-level BEGIN inside `exec_begin` so a prior tx's residue
        // never leaks into this one.
        match exec_begin_or_savepoint(nested, isolation_level.as_deref()).await {
            Ok(savepoint) => {
                // Hand back a continuation that mints the tx-view, calls
                // the creator callback, and attaches commit/rollback
                // handlers — all inside the pump's V8 scope.
                let finalizer = TxFinalizer {
                    outer: outer_global,
                    request_id,
                    savepoint,
                    settled: Cell::new(false),
                };
                OpResult::JsValue {
                    // Throwaway resolver — the Continuation settles its
                    // own (the one inside `finalizer`). We reuse the
                    // outer resolver's Global only to keep the envelope
                    // populated; the pump never touches it on the
                    // Continuation arm.
                    resolver: finalizer.outer.clone(),
                    value: ResolveValue::Continuation(Box::new(move |scope, state| {
                        run_begin_continuation(scope, state, user_fn_global, finalizer, app_id);
                    })),
                    request_id,
                }
            }
            Err(e) => {
                // BEGIN / SAVEPOINT itself failed — nothing to roll back.
                // Top-level BEGIN failures carry `begin_failed`; savepoint
                // failures keep their underlying coded error.
                let coded = if nested {
                    e
                } else {
                    DbError::Coded {
                        code: "begin_failed".to_string(),
                        message: format!("db.transaction: BEGIN failed: {}", e.message_str()),
                        hint: None,
                    }
                };
                OpResult::JsValue {
                    resolver: outer_global,
                    value: ResolveValue::RejectError(coded.to_op_error()),
                    request_id,
                }
            }
        }
    }));

    outer_promise
}

/// Reject an outer resolver synchronously (used for the pre-flight
/// `savepoint_depth_exceeded` refusal, before any SQL runs).
fn reject_outer_now(
    scope: &mut v8::PinScope<'_, '_>,
    outer: &v8::Global<v8::PromiseResolver>,
    err: DbError,
) {
    let resolver = v8::Local::new(scope, outer);
    let op_err = err.to_op_error();
    let exc = op_err.to_exception(scope);
    resolver.reject(scope, exc);
}

// ---------------------------------------------------------------------------
// exec_begin_or_savepoint — the async begin/savepoint SQL
// ---------------------------------------------------------------------------

/// Run `BEGIN [ISOLATION LEVEL ...]` (top-level) or `SAVEPOINT zs_sp_<N>`
/// (nested). Returns `Ok(None)` for a top-level begin, `Ok(Some(name))`
/// for the savepoint name opened on a nested begin.
async fn exec_begin_or_savepoint(
    nested: bool,
    isolation_level: Option<&str>,
) -> Result<Option<String>, DbError> {
    if nested {
        // A savepoint reuses the open connection. The depth counter is
        // the source of the savepoint name; bump it, then emit the SQL.
        // (We bump first so a concurrent finalizer can never observe a
        // name that wasn't yet allocated — V8 is single-threaded so
        // there is no real race, but the ordering keeps the invariant
        // legible.)
        let depth = crate::context::with_mut(|c| c.push_savepoint());
        let name = savepoint_name(depth);
        let sql = format!("SAVEPOINT {name}");
        if let Err(e) = run_on_tx_conn(&sql).await {
            // SAVEPOINT failed — undo the depth bump so the slot stays
            // consistent (the enclosing tx is untouched; nothing was
            // opened).
            crate::context::with_mut(|c| c.pop_savepoint());
            return Err(e);
        }
        return Ok(Some(name));
    }

    // Top-level BEGIN — acquire a dedicated connection (PG-bound, like
    // the pre-P9 begin_transaction / auto_tx paths).
    let begin_sql = build_begin_sql(isolation_level)?;

    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: not configured"))?;
    let pg = backend
        .as_postgres()
        .ok_or_else(|| DbError::backend_unsupported("transaction"))?;
    let client = pg.acquire_dedicated_client().await?;

    client
        .execute(&begin_sql, &[])
        .await
        .map_err(|e| DbError::from_pg(&e))?;

    crate::context::with_mut(|c| {
        let _previous = c.install_tx_client(client);
        debug_assert!(
            _previous.is_none(),
            "exec_begin_or_savepoint: tx_conn slot already occupied"
        );
        c.reset_savepoint_depth();
    });
    // Defensive: drop any broker residue from an interrupted prior run so
    // it cannot leak into this tx's drain.
    clear_pending_emits();
    Ok(None)
}

/// Build the `BEGIN [ISOLATION LEVEL ...]` statement, validating the
/// (already-normalised) isolation string defensively.
fn build_begin_sql(isolation_level: Option<&str>) -> Result<String, DbError> {
    match isolation_level {
        Some(level) => {
            let upper = level.to_uppercase();
            if !VALID_ISOLATION_LEVELS.contains(&upper.as_str()) {
                return Err(DbError::validation(
                    "invalid_isolation_level",
                    format!(
                        "db.transaction: invalid isolation level: {level}. Must be one of: \
                         read uncommitted, read committed, repeatable read, serializable"
                    ),
                ));
            }
            Ok(format!("BEGIN ISOLATION LEVEL {upper}"))
        }
        None => Ok("BEGIN".to_string()),
    }
}

/// Savepoint identifier for nesting `depth` (1-based). `zs_sp_1`,
/// `zs_sp_2`, … — the `zs_` prefix keeps the name out of any plausible
/// user-chosen savepoint namespace.
fn savepoint_name(depth: u32) -> String {
    format!("zs_sp_{depth}")
}

/// Run a single non-returning statement (`SAVEPOINT` / `RELEASE` /
/// `ROLLBACK TO` / `COMMIT` / `ROLLBACK`) against the pinned tx
/// connection, holding the client across the await and putting it back.
/// Used for savepoint statements that must NOT drain the connection.
async fn run_on_tx_conn(sql: &str) -> Result<(), DbError> {
    let client = crate::context::with_mut(|c| c.take_tx_client())
        .ok_or_else(|| DbError::internal("db: transaction connection lost"))?;
    let result = client.execute(sql, &[]).await;
    // Put the client back regardless — savepoint statements keep the tx
    // open.
    crate::context::with_mut(|c| c.put_tx_client(client));
    result.map(|_| ()).map_err(|e| DbError::from_pg(&e))
}

// ---------------------------------------------------------------------------
// run_begin_continuation — mint view, call callback, attach handlers
// ---------------------------------------------------------------------------

/// The continuation the pump runs after a successful BEGIN / SAVEPOINT.
/// Mints the tx-view, calls the creator callback (capturing a synchronous
/// throw), coerces the return to a Promise, and attaches the
/// commit/rollback handlers.
fn run_begin_continuation(
    scope: &mut v8::PinScope<'_, '_>,
    state: &SharedState,
    user_fn_global: v8::Global<v8::Function>,
    finalizer: TxFinalizer,
    app_id: String,
) {
    // 1. Mint the tx-view (collections-as-props; no commit/rollback).
    let tx_view = match crate::v8_classes::transaction::mint_tx_view(scope, &app_id) {
        Ok(v) => v,
        Err(e) => {
            // Minting failed before the callback ran — roll the tx back
            // and reject the outer promise. There is no body promise yet,
            // so settle directly. (`mint_tx_view` already returns an
            // `OpError`.)
            settle_failed_before_body(scope, state, finalizer, e);
            return;
        }
    };

    // 2. Call the creator callback inside a TryCatch to capture a
    //    *synchronous* throw (e.g. a non-async callback that throws, or
    //    an async callback that throws before its first await).
    let user_fn = v8::Local::new(scope, &user_fn_global);
    let undefined = v8::undefined(scope).into();
    let call_result = {
        v8::tc_scope!(let tc, scope);
        let ret = user_fn.call(tc, undefined, &[tx_view.into()]);
        match ret {
            Some(v) => Ok(v8::Global::new(tc, v)),
            None => {
                // Synchronous throw — capture the exception verbatim.
                let exc = tc
                    .exception()
                    .map(|e| v8::Global::new(tc, e));
                Err(exc)
            }
        }
    };

    let ret_global = match call_result {
        Ok(v) => v,
        Err(exc) => {
            // Callback threw synchronously → straight to rollback, then
            // reject the outer with the captured exception.
            let op_err = exc.map_or_else(
                || crate::error::DbError::internal("db.transaction: callback threw").to_op_error(),
                |g| {
                    let local = v8::Local::new(scope, &g);
                    zeroship_runtime::state::OpError::js_value(
                        scope,
                        local,
                        "db.transaction: callback threw",
                    )
                },
            );
            settle_failed_before_body(scope, state, finalizer, op_err);
            return;
        }
    };

    // 3. Coerce the return value to a Promise.
    let ret_local = v8::Local::new(scope, &ret_global);
    let body_promise = coerce_to_promise(scope, ret_local);

    // 4. Attach commit / rollback handlers carrying the finalizer.
    attach_tx_finalizer(scope, body_promise, finalizer);
}

/// Coerce a callback return value to a Promise:
///   - already a Promise → use it directly;
///   - any other value → wrap in an immediately-resolved Promise.
///
/// (A synchronous *throw* is handled earlier, in
/// [`run_begin_continuation`], before this is reached.)
fn coerce_to_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    if value.is_promise() {
        return value.try_into().expect("is_promise() guaranteed a Promise");
    }
    // Wrap a plain value in a resolved Promise so the `.then` machinery
    // (and therefore COMMIT) fires uniformly.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    resolver.resolve(scope, value);
    promise
}

/// Attach Rust-backed resolve / reject handlers to the creator's body
/// promise. Each handler's `.data()` is a `v8::External` pointing at the
/// boxed [`TxFinalizer`]; the handler reads it back via `args.data()`.
///
/// Mirrors the Promise-attached-handler pattern the runtime already uses
/// for async continuations (e.g. `fetch`/`waitUntil` settle promises from
/// Rust); the novel bit here is carrying owned Rust state into the
/// handler through `FunctionBuilder::data` + `v8::External` rather than a
/// captured closure (V8 function callbacks are bare `fn`s).
fn attach_tx_finalizer(
    scope: &mut v8::PinScope<'_, '_>,
    body_promise: v8::Local<v8::Promise>,
    finalizer: TxFinalizer,
) {
    // Heap the finalizer; both handlers share the same box. Whichever
    // handler fires reclaims it (V8 settles a promise exactly once).
    let boxed = Box::new(finalizer);
    let raw = Box::into_raw(boxed);
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    let data: v8::Local<v8::Value> = ext.into();

    let on_fulfilled = v8::Function::builder(tx_resolve_handler)
        .data(data)
        .build(scope)
        .expect("build tx resolve handler");
    let on_rejected = v8::Function::builder(tx_reject_handler)
        .data(data)
        .build(scope)
        .expect("build tx reject handler");

    // then2 attaches both handlers; the returned derived promise is
    // intentionally dropped (we settle the outer resolver ourselves, so
    // the derived promise has no observer).
    let _ = body_promise.then2(scope, on_fulfilled, on_rejected);
}

// ---------------------------------------------------------------------------
// V8 handler callbacks — reclaim the finalizer, spawn the settle op
// ---------------------------------------------------------------------------

/// `.then` fulfilment handler — the creator callback resolved. Reclaims
/// the [`TxFinalizer`] and spawns the COMMIT (top-level) or RELEASE
/// SAVEPOINT (nested) op, which resolves the outer promise with the body
/// result.
fn tx_resolve_handler(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let body_value = args.get(0);
    settle_after_body(scope, &args, Some(v8::Global::new(scope, body_value)), None);
    rv.set(v8::undefined(scope).into());
}

/// `.then` rejection handler — the creator callback threw / rejected.
/// Reclaims the [`TxFinalizer`] and spawns the ROLLBACK (top-level) or
/// ROLLBACK TO SAVEPOINT (nested) op, which rejects the outer promise
/// with the body error.
fn tx_reject_handler(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let body_error = args.get(0);
    settle_after_body(scope, &args, None, Some(v8::Global::new(scope, body_error)));
    rv.set(v8::undefined(scope).into());
}

/// Shared body of the resolve / reject handlers. Exactly one of
/// `body_ok` / `body_err` is `Some`. Reclaims the finalizer box and
/// spawns the appropriate settle op.
fn settle_after_body(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
    body_ok: Option<v8::Global<v8::Value>>,
    body_err: Option<v8::Global<v8::Value>>,
) {
    // Recover the boxed finalizer from the function's data slot.
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        // Data wasn't an External — should be impossible (we always
        // attach one). Bail without touching memory.
        return;
    };
    let raw = ext.value() as *mut TxFinalizer;
    if raw.is_null() {
        return;
    }
    // SAFETY: `raw` was `Box::into_raw`'d in `attach_tx_finalizer`. V8
    // calls exactly one of the two handlers, exactly once, so taking
    // ownership here reclaims the box exactly once; the other handler's
    // identical `External` is never dereferenced (its promise branch
    // didn't fire). The `settled` flag guards a pathological double-call.
    let finalizer: Box<TxFinalizer> = unsafe { Box::from_raw(raw) };
    if finalizer.settled.replace(true) {
        // Already settled — re-leak the box (do not double-free) and
        // return. `Box::into_raw` here cancels the `from_raw` above.
        let _ = Box::into_raw(finalizer);
        return;
    }

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let TxFinalizer {
        outer,
        request_id,
        savepoint,
        ..
    } = *finalizer;

    let success = body_ok.is_some();
    let body = if success { body_ok } else { body_err };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let settle_result = exec_settle(success, savepoint.as_deref()).await;
        let value = build_settle_resolve_value(settle_result, success, body);
        OpResult::JsValue {
            resolver: outer,
            value,
            request_id,
        }
    }));
}

/// Settle the outer promise directly (no body promise was created — the
/// callback threw synchronously or the tx-view mint failed). Rolls the tx
/// back and rejects.
fn settle_failed_before_body(
    scope: &mut v8::PinScope,
    state: &SharedState,
    finalizer: TxFinalizer,
    op_err: zeroship_runtime::state::OpError,
) {
    if finalizer.settled.replace(true) {
        return;
    }
    let TxFinalizer {
        outer,
        request_id,
        savepoint,
        ..
    } = finalizer;
    let err_global = {
        // Materialise the JS exception now, while we have a scope, so the
        // async settle op can reject with the real value.
        let exc = op_err.to_exception(scope);
        v8::Global::new(scope, exc)
    };
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Roll back (best-effort); the body already failed.
        let _ = exec_settle(false, savepoint.as_deref()).await;
        OpResult::JsValue {
            resolver: outer,
            value: ResolveValue::Reject(err_global),
            request_id,
        }
    }));
}

// ---------------------------------------------------------------------------
// exec_settle — COMMIT / ROLLBACK / RELEASE / ROLLBACK TO
// ---------------------------------------------------------------------------

/// Result of the settle SQL, carrying enough to build the outer promise's
/// `ResolveValue`.
enum SettleOutcome {
    /// Settle SQL succeeded.
    Ok,
    /// COMMIT failed after the body resolved — the tx state is
    /// indeterminate; reject with `commit_failed_indeterminate`.
    CommitIndeterminate(DbError),
    /// A rollback / release / rollback-to failed. The body's own
    /// success/failure still governs how the outer settles, but a failed
    /// commit-path rollback escalates to an error.
    SettleErr(DbError),
}

/// Run the settle statement for this transaction level.
///
/// Top-level (`savepoint == None`):
///   - success → `COMMIT`, drop the connection, drain pending emits.
///   - failure → `ROLLBACK`, drop the connection, clear pending emits.
/// Nested (`savepoint == Some(name)`):
///   - success → `RELEASE SAVEPOINT name` (keeps the connection open).
///   - failure → `ROLLBACK TO SAVEPOINT name` (keeps the connection
///     open; the enclosing tx continues).
async fn exec_settle(success: bool, savepoint: Option<&str>) -> SettleOutcome {
    match savepoint {
        Some(name) => {
            // Nested — pop the depth first so a sibling/enclosing level
            // sees the correct count, then run RELEASE / ROLLBACK TO.
            crate::context::with_mut(|c| c.pop_savepoint());
            let sql = if success {
                format!("RELEASE SAVEPOINT {name}")
            } else {
                format!("ROLLBACK TO SAVEPOINT {name}")
            };
            match run_on_tx_conn(&sql).await {
                Ok(()) => SettleOutcome::Ok,
                Err(e) => SettleOutcome::SettleErr(e),
            }
        }
        None => exec_settle_top_level(success).await,
    }
}

/// Top-level COMMIT / ROLLBACK. Drains the connection out of the slot,
/// runs the statement, drops the client, and settles the broker queue.
async fn exec_settle_top_level(success: bool) -> SettleOutcome {
    let client_opt = crate::context::with_mut(|c| c.take_tx_client());
    let Some(client) = client_opt else {
        // Slot already drained (e.g. a concurrent teardown). Treat as
        // settled — clear residual state.
        crate::context::with_mut(|c| c.reset_savepoint_depth());
        clear_pending_emits();
        return SettleOutcome::Ok;
    };

    let cmd = if success { "COMMIT" } else { "ROLLBACK" };
    let result = client.execute(cmd, &[]).await;
    drop(client);

    // Clear the tx slot bookkeeping now that the connection is gone.
    crate::context::with_mut(|c| c.reset_savepoint_depth());

    match (success, result) {
        (true, Ok(_)) => {
            // Commit succeeded — fire the deferred broker events.
            drain_pending_emits_on_commit();
            SettleOutcome::Ok
        }
        (true, Err(e)) => {
            // COMMIT failed after the body resolved → indeterminate.
            // Best-effort rollback is moot (the connection is gone, which
            // Postgres treats as a rollback). Drop the queued events so
            // subscribers never see writes that may not have landed.
            clear_pending_emits();
            SettleOutcome::CommitIndeterminate(DbError::from_pg(&e))
        }
        (false, Ok(_)) => {
            // Rollback succeeded — drop the queued events.
            clear_pending_emits();
            SettleOutcome::Ok
        }
        (false, Err(_)) => {
            // Rollback failed; the connection drop already aborted the tx
            // server-side. Treat as rolled back (the body error still
            // governs the outer rejection).
            clear_pending_emits();
            SettleOutcome::Ok
        }
    }
}

/// Lower the settle outcome + body value into the `ResolveValue` that
/// settles the outer promise.
fn build_settle_resolve_value(
    outcome: SettleOutcome,
    body_success: bool,
    body: Option<v8::Global<v8::Value>>,
) -> ResolveValue {
    match outcome {
        SettleOutcome::Ok => {
            if body_success {
                // Resolve the outer promise with the body's resolved
                // value.
                match body {
                    Some(g) => ResolveValue::JsGlobal(g),
                    None => ResolveValue::Undefined,
                }
            } else {
                // Body rejected → reject the outer with the body error.
                match body {
                    Some(g) => ResolveValue::Reject(g),
                    None => ResolveValue::RejectError(
                        DbError::internal("db.transaction: rolled back").to_op_error(),
                    ),
                }
            }
        }
        SettleOutcome::CommitIndeterminate(e) => {
            ResolveValue::RejectError(
                DbError::Coded {
                    code: "commit_failed_indeterminate".to_string(),
                    message: format!(
                        "commit failed — transaction state indeterminate: {}",
                        e.message_str()
                    ),
                    hint: None,
                }
                .to_op_error(),
            )
        }
        SettleOutcome::SettleErr(e) => {
            // A nested RELEASE / ROLLBACK TO failed. Surface it as a coded
            // error regardless of the body outcome — the savepoint state
            // is no longer trustworthy.
            ResolveValue::RejectError(e.to_op_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn savepoint_name_is_prefixed_and_1_based() {
        assert_eq!(savepoint_name(1), "zs_sp_1");
        assert_eq!(savepoint_name(2), "zs_sp_2");
        assert_eq!(savepoint_name(8), "zs_sp_8");
    }

    #[test]
    fn build_begin_sql_plain_and_isolation() {
        assert_eq!(build_begin_sql(None).unwrap(), "BEGIN");
        assert_eq!(
            build_begin_sql(Some("SERIALIZABLE")).unwrap(),
            "BEGIN ISOLATION LEVEL SERIALIZABLE"
        );
        assert_eq!(
            build_begin_sql(Some("read committed")).unwrap(),
            "BEGIN ISOLATION LEVEL READ COMMITTED"
        );
    }

    #[test]
    fn build_begin_sql_rejects_unknown_level() {
        let err = build_begin_sql(Some("bananas")).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_isolation_level");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn max_savepoint_depth_is_eight() {
        // Pin the proposal's depth cap so a future change is a deliberate
        // edit, not an accidental drift.
        assert_eq!(MAX_SAVEPOINT_DEPTH, 8);
    }

    #[test]
    fn build_settle_resolve_value_commit_indeterminate_has_code() {
        let rv = build_settle_resolve_value(
            SettleOutcome::CommitIndeterminate(DbError::internal("network drop")),
            true,
            None,
        );
        match rv {
            ResolveValue::RejectError(op_err) => match op_err.kind {
                zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
                    assert_eq!(code, "commit_failed_indeterminate");
                }
                other => panic!("expected CodedError, got {other:?}"),
            },
            _ => panic!("expected RejectError"),
        }
    }

    #[test]
    fn build_settle_resolve_value_ok_resolve_undefined_when_no_body() {
        let rv = build_settle_resolve_value(SettleOutcome::Ok, true, None);
        matches!(rv, ResolveValue::Undefined)
            .then_some(())
            .expect("expected Undefined for ok+no-body");
    }
}
