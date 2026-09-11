//! V8 callback integration for ORM transactions.
//!
//! The adapter captures async scope, invokes creator callbacks with collection
//! handles, and reports callback completion to the ORM orchestrator. Transaction
//! generation and savepoint identity travel with the callback's scope.
//!
//! Creator callbacks resolve to commit or throw to roll back. The ORM owns session
//! acquisition, SQL settlement and cancellation cleanup.

#![allow(unsafe_code)]

use std::cell::Cell;

use zeroship_runtime::state::{OpError, OpResult, ResolveValue, SharedState};

use crate::op_error::ToOpError;
use crate::transaction::{
    SettleOutcome, TxAdmission, exec_begin_or_savepoint, exec_settle, reducer,
};
use crate::v8_bridge::runtime_state;
use crate::v8_classes::collection::mint_collection;
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::{DbError, IsolationLevel};

/// Mint the collections-only `tx` view for a `Db.transaction(fn)`
/// callback.
///
/// Builds a fresh `v8::Object` and sets one
/// [`Collection`](super::collection::Collection) property per collection
/// the per-thread schema cache knows about for this binding (the same set
/// native runtime boot publishes). Each minted `Collection` is an
/// ordinary v8_class instance — identical to what `db.collection(name)`
/// returns — so its CRUD methods route through the active transaction
/// connection via the `tx_conn` slot the orchestrator set before calling
/// the creator callback.
///
/// No `commit` / `rollback` / `collection` / `transaction` / `live`
/// method is set on the view: the only members are collections. Manual
/// abort = throw inside the callback; commit is implicit on resolve.
///
/// When the descriptor store holds no entry for this binding, as with a raw-JS
/// schema-less deploy, the view is an empty object. A transaction with no
/// declared collections has nothing to address through `tx.<name>`; the
/// commit/rollback envelope still applies.
pub(crate) fn mint_tx_view<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: &DbBinding,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let view = v8::Object::new(scope);

    // One tx-bound Collection per cached-schema collection. The list
    // mirrors `Db::collection(name)`'s minting; the binding to the open
    // tx is implicit (the `tx_conn` slot is set), so no per-object tx id
    // is threaded.
    let collections: Vec<String> = crate::descriptor::declared_collections(binding)
        .into_iter()
        .map(|(name, _schema)| name)
        .collect();

    for name in collections {
        let col = mint_collection(scope, name.clone(), binding.clone())?;
        let key = v8::String::new(scope, &name)
            .ok_or_else(|| OpError::type_error("tx-view: collection name allocation failed"))?;
        view.set(scope, key.into(), col.into());
    }

    Ok(view)
}

// ---------------------------------------------------------------------------
// The V8 half of `Db.transaction(fn)`
// ---------------------------------------------------------------------------
//
// These eleven items lived in `crate::transaction` until 2026-09-02. They are
// the promise machinery: mint the outer resolver, run the creator callback
// inside a `TryCatch`, attach native then/catch handlers whose `.data()`
// carries a boxed `TxFinalizer`, and lower a settle outcome into a
// `ResolveValue`.
//
// **The SC-1 protocol did not move with them, and that is the point.** The
// reducer, the admission claim, the frame stack, the deadline and the driver
// stay in `crate::transaction`, which now names no `v8::` type in any
// signature. What crosses between the two is data: an app id, a `FrameId`, a
// `SettleOutcome`. The eleven were picked by exactly that test - does the
// signature (or, for `TxFinalizer`, a field) name a `v8::` type - which is why
// `exec_begin_or_savepoint` and `exec_settle` stayed behind despite being the
// things these functions call.

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
    /// (settle = `COMMIT` / `ROLLBACK`, then dispose of the session).
    /// `Some(frame)` ⇒ this `transaction()` opened a nested `SAVEPOINT`
    /// (settle = `RELEASE` / `ROLLBACK TO` + `RELEASE`, connection stays open
    /// for the enclosing tx).
    ///
    /// A [`reducer::frames::FrameId`], not a savepoint NAME. The name lives on
    /// the reducer's frame stack, is minted from a monotonic sequence, and is
    /// never reused - so a settle can never name a savepoint some other frame
    /// established. Carrying the name here is how a depth-derived scheme sends
    /// a rollback to the wrong scope.
    frame: Option<reducer::frames::FrameId>,
    /// Owning app. SEC-1: the settle path (COMMIT / ROLLBACK / RELEASE /
    /// ROLLBACK TO + pending-emit drain) operates strictly on this app's
    /// slot, so one app's transaction can never settle another's.
    app_id: String,
    /// One-shot guard. Set the first time either handler fires.
    settled: Cell<bool>,
}
/// `env.db.transaction(asyncFn, opts?)` → `Promise<R>`.
///
/// Synchronously mints the outer promise + decides BEGIN-vs-SAVEPOINT,
/// then spawns the begin/savepoint op. Returns the outer promise; the
/// creator callback runs once the begin op completes (see the module
/// comment for the full 8-step flow). `asyncFn` is validated to be a
/// function here; `opts.isolationLevel` is resolved by the caller
/// (`Db::transaction`) and arrives as a variant, so nothing downstream can
/// receive a spelling it has to interpret.
pub fn transaction_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    user_fn: v8::Local<v8::Function>,
    isolation_level: Option<IsolationLevel>,
    binding: DbBinding,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id().to_string();
    // SCHEMA: the PostgreSQL session this BEGIN opens narrows to the role
    // derived from it. `app_id` above stays the SC-1 admission key.
    let schema = binding.schema().clone();
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

    // Decide BEGIN vs SAVEPOINT from the calling frame's ASYNC CONTEXT,
    // not from whether the app happens to have a transaction open.
    //
    // Until 2026-08-10 this read `has_tx_for(app_id)` — "does this app
    // have a tx open right now?". That is a temporal test standing in for
    // a structural one, and it is wrong whenever two transactions for one
    // app overlap in time, which they routinely do: a worker thread
    // multiplexes many requests over one isolate and yields at every
    // `.await`, and `pnpm dev` is one isolate by construction. An
    // unrelated request's `transaction()` read `true`, opened a SAVEPOINT
    // on the FIRST request's connection, reported success, and then lost
    // its row to the first request's ROLLBACK. Measured on both tiers by
    // `examples/db-todos/tests/database.test.ts` (`cxOvl`).
    //
    // `current_tx_app` is true only inside the enclosing callback's own
    // continuation chain — see `crate::tx_scope`. SEC-1 still holds and is
    // now structural rather than incidental: a co-resident app's callback
    // plants ITS app_id, so the comparison below fails and this app opens
    // its own top-level BEGIN.
    let parent_scope = crate::tx_scope::current_tx_scope(scope)
        .filter(|parent| parent.app_id() == app_id);
    let nested = parent_scope.is_some();
    if let Some(parent) = &parent_scope {
        if let Err(error) = parent.check() {
            reject_outer_now(scope, &outer_global, error);
            return outer_promise;
        }
    }

    // The savepoint-depth cap is NOT re-checked here. `FrameStack::open_child`
    // refuses the (MAX+1)-th simultaneous frame before any `SAVEPOINT` reaches
    // the wire and answers `savepoint_depth_exceeded`, which is the same code
    // this used to produce. A second copy of the rule beside the state machine
    // is a copy that can disagree with it.

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Drain note: the JS-side DataLoader queues are flushed by the
        // bootstrap wrapper *before* it calls this native method (those
        // microtask queues are pure-JS state with no Rust counterpart).
        // The Rust-side broker `pending_emits` queue is cleared on every
        // top-level BEGIN inside `exec_begin` so a prior tx's residue
        // never leaks into this one.
        // Serialise top-level transactions for this app on this isolate.
        // Only ONE tx connection slot exists per (app, isolate), so a
        // second concurrent top-level transaction has nowhere to live: it
        // used to evict the first (Postgres) or be refused by the single
        // SQLite writer with `cannot start a transaction within a
        // transaction`. Waiting turns both of those into "runs second and
        // succeeds", which is what a creator writing
        // `Promise.all([db.transaction(a), db.transaction(b)])` means.
        //
        // Cannot deadlock: a genuinely NESTED call skips this (it does not
        // need a claim), and the claim holder never waits on a waiter.
        //
        // **The cancellation window is closed by [`TxAdmission`], not left
        // open.** This comment used to end "if this op is CANCELLED between
        // taking the claim and the `BEGIN` returning, the claim leaks ...
        // closing the window needs an RAII guard armed for exactly this window
        // and disarmed once the client is installed; it is not here because it
        // is unverified". That guard is now here, and it is SC-1 rule 5 - the
        // defect labelled DBR-11.
        let admission = if nested {
            None
        } else {
            Some(TxAdmission::acquire(app_id.clone()).await)
        };
        // Resolved adapter-side. A resolution failure folds into the same
        // `Err` arm as a begin failure, so the admission claim is dropped and
        // released on that path too.
        //
        // BEHAVIOUR CHANGE, stated rather than discovered: a cold
        // `initialize_backend` now runs HERE, after `TxAdmission::acquire` but
        // before the BEGIN, where it used to run deeper inside `open_session`.
        // It shortens the window the claim is held across, but it is a change
        // to admission timing, not a refactor.
        let began = match crate::tx_scope::ensure_backend().await {
            Ok(backend) => {
                match parent_scope.as_ref().map_or(Ok(()), |parent| parent.check()) {
                    Ok(()) => exec_begin_or_savepoint(nested, isolation_level, &app_id, schema, backend).await,
                    Err(error) => Err(error),
                }
            }
            Err(e) => Err(e),
        };
        match began {
            Ok(frame) => {
                // The session is installed and the reducer is in `Idle`. Every
                // path out of there emits `ReleaseAdmission`, so the claim's
                // release is now the protocol's rather than this future's - and
                // an unsettling callback is bounded by the execution deadline
                // rather than by nothing.
                if let Some(admission) = admission {
                    admission.handed_to_reducer();
                }
                // Hand back a continuation that mints the tx-view, calls
                // the creator callback, and attaches commit/rollback
                // handlers — all inside the pump's V8 scope.
                let finalizer = TxFinalizer {
                    outer: outer_global,
                    request_id,
                    frame,
                    app_id: app_id.clone(),
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
                        run_begin_continuation(scope, state, user_fn_global, finalizer, binding);
                    })),
                    request_id,
                }
            }
            Err(e) => {
                // `admission` is still armed and drops here, which releases the
                // claim, retires the reducer and destroys any session that was
                // installed. `exec_begin_or_savepoint` has already wrapped only
                // an unclassified top-level failure as `begin_failed`; every
                // classified setup error and every savepoint error stays exact.
                drop(admission);
                OpResult::JsValue {
                    resolver: outer_global,
                    value: ResolveValue::RejectError(e.to_op_error()),
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
/// The continuation the pump runs after a successful BEGIN / SAVEPOINT.
/// Mints the tx-view, calls the creator callback (capturing a synchronous
/// throw), coerces the return to a Promise, and attaches the
/// commit/rollback handlers.
fn run_begin_continuation(
    scope: &mut v8::PinScope<'_, '_>,
    state: &SharedState,
    user_fn_global: v8::Global<v8::Function>,
    finalizer: TxFinalizer,
    binding: DbBinding,
) {
    let app_id = binding.app_id();
    // 1. Mint the tx-view (collections-as-props; no commit/rollback).
    let tx_view = match crate::v8_classes::transaction::mint_tx_view(scope, &binding) {
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
    //    The call is wrapped in the async-scope marker: every continuation
    //    that branches off inside the callback inherits `app_id` in V8's
    //    continuation-preserved slot, so a `db.transaction()` reached from
    //    in there reads as NESTED while a concurrent dispatch's does not.
    //    See `crate::tx_scope`. Restored on both exit paths below —
    //    leaving it set would make the NEXT unrelated dispatch on this
    //    isolate think it was inside this transaction.
    let user_fn = v8::Local::new(scope, &user_fn_global);
    let undefined = v8::undefined(scope).into();
    let callback_scope = match zeroship_data_orm::transaction::scope::TransactionScope::current(app_id) {
        Ok(callback_scope) => callback_scope,
        Err(error) => {
            settle_failed_before_body(scope, state, finalizer, error.to_op_error());
            return;
        }
    };
    let prev_scope = crate::tx_scope::enter(scope, &callback_scope);
    let call_result = {
        v8::tc_scope!(let tc, scope);
        let ret = user_fn.call(tc, undefined, &[tx_view.into()]);
        match ret {
            Some(v) => Ok(v8::Global::new(tc, v)),
            None => {
                // Synchronous throw — capture the exception verbatim.
                let exc = tc.exception().map(|e| v8::Global::new(tc, e));
                Err(exc)
            }
        }
    };
    crate::tx_scope::leave(scope, prev_scope);

    let ret_global = match call_result {
        Ok(v) => v,
        Err(exc) => {
            // Callback threw synchronously → straight to rollback, then
            // reject the outer with the captured exception.
            let op_err = exc.map_or_else(
                || {
                    zeroship_data_orm::error::DbError::internal("db.transaction: callback threw")
                        .to_op_error()
                },
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
        frame,
        app_id,
        ..
    } = *finalizer;

    let success = body_ok.is_some();
    let body = if success { body_ok } else { body_err };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let settle_result = exec_settle(&app_id, success, frame).await;
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
        frame,
        app_id,
        ..
    } = finalizer;
    let err_global = {
        // Materialise the JS exception now, while we have a scope, so the
        // async settle op can reject with the real value.
        let exc = op_err.to_exception(scope);
        v8::Global::new(scope, exc)
    };
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let settle_result = exec_settle(&app_id, false, frame).await;
        OpResult::JsValue {
            resolver: outer,
            value: build_settle_resolve_value(settle_result, false, Some(err_global)),
            request_id,
        }
    }));
}
/// Lower the settle outcome + body value into the `ResolveValue` that
/// settles the outer promise.
pub(crate) fn build_settle_resolve_value(
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
        // Both arms surface the error the settle produced, VERBATIM. Neither
        // re-wraps: `driver::outcome_error` already chose the code that names
        // what happened, and wrapping it again in
        // `commit_failed_indeterminate` labelled a definitively rolled-back
        // commit as an unknown one - the opposite of what the state machine
        // established.
        SettleOutcome::CommitIndeterminate(e) | SettleOutcome::SettleErr(e) => {
            ResolveValue::RejectError(e.to_op_error())
        }
    }
}
#[cfg(test)]
mod tests {
    //! Shape guards for the `Db.transaction(fn)` callback argument.
    //!
    //! The proposal (Q-P9-C, §4.4) fixes the tx-view as **collections
    //! only** — no `commit` / `rollback` / `collection` / `transaction` /
    //! `live` method. These tests mint a view directly (no DB needed —
    //! the per-thread schema cache is empty in this test context, so the
    //! view is a bare object) and assert no tx-lifecycle method leaked
    //! onto it. If a future change re-introduces a `commit`/`rollback`
    //! method on the view, these fail.
    #![allow(unsafe_code)]

    use zeroship_runtime::init_v8;

    fn assert_absent(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, name: &str) {
        let key = v8::String::new(scope, name).unwrap();
        let v = obj.get(scope, key.into()).unwrap();
        assert!(
            v.is_undefined(),
            "tx-view must NOT expose `{name}` — it is collections-only \
             (no JS-reachable transaction primitive); got a defined value"
        );
    }

    #[test]
    fn tx_view_has_no_commit_or_rollback_methods() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let binding = crate::tests::fixtures::binding("test_app");
        let view = super::mint_tx_view(scope, &binding).expect("mint_tx_view");

        // None of the legacy `Transaction` methods, nor `transaction` /
        // `live`, may appear on the view.
        for forbidden in [
            "commit",
            "rollback",
            "collection",
            "transaction",
            "live",
            "beginTransaction",
        ] {
            assert_absent(scope, view, forbidden);
        }

        // It is a plain object (its [[Prototype]] is Object.prototype,
        // not some Transaction.prototype carrying methods). Confirm the
        // prototype chain has no `commit`.
        let key = v8::String::new(scope, "commit").unwrap();
        // `get` walks the prototype chain; a plain object's chain ends at
        // Object.prototype which has no `commit`.
        let v = view.get(scope, key.into()).unwrap();
        assert!(
            v.is_undefined(),
            "commit must be absent up the whole prototype chain"
        );
    }

    #[test]
    fn tx_view_is_empty_without_runtime_descriptor() {
        // A schema-less app has no collections in the thread context, so the view has
        // no own enumerable properties. This is the raw-JS-deploy path.
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let binding = crate::tests::fixtures::binding("test_app");
        let view = super::mint_tx_view(scope, &binding).expect("mint_tx_view");
        let names = view
            .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
            .unwrap();
        assert_eq!(
            names.length(),
            0,
            "tx-view for a schema-less app must be empty"
        );
    }

    // ---------------------------------------------------------------------
    // The settle-lowering arms, MOVED here from the engine's
    // `transaction/mod.rs` with the data-engine cut on 2026-09-03.
    //
    // They rule on `build_settle_resolve_value`, which is defined in THIS file
    // and lowers an engine `SettleOutcome` into a runtime `ResolveValue`. They
    // could not travel with the module whose outcomes they check:
    // `zeroship-data-orm` declares neither `v8` nor `zeroship-runtime`, by
    // design, so `ResolveValue` is not nameable there.
    // ---------------------------------------------------------------------

    /// The settle's own code reaches the creator, UNWRAPPED.
    ///
    /// It used to be re-wrapped in `commit_failed_indeterminate`, which
    /// labelled every failing settle "indeterminate" - including a COMMIT the
    /// server answered `ROLLBACK`, whose outcome is not unknown at all. The
    /// codes now come from `driver::outcome_error`, one per outcome.
    #[test]
    fn a_settles_own_code_reaches_the_creator_unwrapped() {
        use crate::transaction::SettleOutcome;
        use zeroship_data_orm::error::DbError;
        use zeroship_runtime::state::ResolveValue;

        for outcome in [
            SettleOutcome::CommitIndeterminate(DbError::Coded {
                code: "commit_failed_indeterminate".to_string(),
                message: "network drop".to_string(),
                hint: None,
            }),
            SettleOutcome::SettleErr(DbError::Coded {
                code: "commit_rolled_back".to_string(),
                message: "the server discarded it".to_string(),
                hint: None,
            }),
        ] {
            let expected = match &outcome {
                SettleOutcome::CommitIndeterminate(_) => "commit_failed_indeterminate",
                SettleOutcome::SettleErr(_) => "commit_rolled_back",
                SettleOutcome::Ok => unreachable!(),
            };
            match super::build_settle_resolve_value(outcome, true, None) {
                ResolveValue::RejectError(op_err) => match op_err.kind {
                    zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
                        assert_eq!(
                            code, expected,
                            "the settle's code must survive; re-wrapping it makes \
                             every failure read as indeterminate"
                        );
                    }
                    other => panic!("expected CodedError, got {other:?}"),
                },
                _ => panic!("expected RejectError for {expected}"),
            }
        }
    }

    #[test]
    fn build_settle_resolve_value_ok_resolve_undefined_when_no_body() {
        use crate::transaction::SettleOutcome;
        use zeroship_runtime::state::ResolveValue;

        let rv = super::build_settle_resolve_value(SettleOutcome::Ok, true, None);
        matches!(rv, ResolveValue::Undefined)
            .then_some(())
            .expect("expected Undefined for ok+no-body");
    }
}
