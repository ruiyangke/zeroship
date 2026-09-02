//! Native `Db.transaction(asyncFn, opts?)` orchestrator.
//!
//! Transaction orchestration lives **entirely in Rust**.
//! The creator API is unchanged — `await env.db.transaction(async tx =>
//! {...})` commits on resolve, rolls back on throw — but the begin /
//! commit / rollback / nested-savepoint state machine moved out of the
//! bootstrap's JS `transactionImpl` and into [`transaction_dispatch`].
//! `db.beginTransaction()` and the `Transaction` v8_class methods
//! (`commit`/`rollback`/`collection`) no longer exist on the JS surface.
//!
//! ## The orchestrator runs on the SC-1 reducer
//!
//! This module owns the V8 shape - promises, continuations, `.then` handlers -
//! and **nothing else**. Every state transition is an event applied to
//! [`reducer::TxReducer`], and every statement that reaches the wire is an
//! [`reducer::Action`] [`driver`] was told to issue. There is no second copy of
//! the rules here: the depth cap, the savepoint names, the effect fate, the
//! session disposition and the admission release all live in the state machine.
//!
//! 1. [`transaction_dispatch`] (a sync v8_method body) mints the outer
//!    [`v8::PromiseResolver`] and returns its promise to JS immediately.
//! 2. It reads the calling frame's **async context**
//!    ([`crate::tx_scope`]) to decide whether this is a **top-level**
//!    transaction (not inside any transaction callback → admit a reducer and
//!    emit `BEGIN`) or a **nested** one (inside this app's enclosing callback →
//!    open a frame and emit `SAVEPOINT`). It is deliberately NOT "does this app
//!    have a transaction open right now" — that test cannot tell a nested call
//!    from an unrelated concurrent one, and reading it that way silently folded
//!    one request's transaction into another's (see [`crate::tx_scope`] for the
//!    measurement).
//! 3. A spawned op takes the admission claim as an RAII [`TxAdmission`] guard
//!    and runs the `BEGIN` / `SAVEPOINT` through the reducer. On success it
//!    hands back a [`zeroship_runtime::state::ResolveValue::Continuation`]; on
//!    failure the guard's drop releases the claim, retires the reducer and
//!    withdraws any session that was installed.
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
//!    carries a heap [`TxFinalizer`] (the outer resolver + the FRAME ID + the
//!    request id — never a savepoint name).
//! 7. On the creator promise **resolving**, [`tx_resolve_handler`] settles:
//!    `COMMIT` at the root, `RELEASE` for a frame.
//! 8. On the creator promise **rejecting**, [`tx_reject_handler`] settles:
//!    `ROLLBACK` at the root, `ROLLBACK TO` + `RELEASE` for a frame.
//!
//! ## Two defects this shape removes, by name
//!
//! - **DBR-03**, "an absent client is proof terminal SQL ran". The settle path
//!   used to read an empty transaction slot as "already settled", release the
//!   claim and return success **without sending anything**. Only `Settled` ends
//!   a settle early now; a settle that arrives while an operation owns the
//!   session waits in `Quiescing`, and a session that is genuinely unreachable
//!   when terminal SQL is due settles as indeterminate and withdraws.
//! - **DBR-11**, the leaked admission claim. Cancellation between taking the
//!   claim and `BEGIN` returning left it held forever. [`TxAdmission`] covers
//!   exactly that window, and from `Idle` onward the reducer emits
//!   `ReleaseAdmission` on every path to `Settled`.
//!
//! Savepoint names are the frame stack's monotonic sequence, never
//! `zs_sp_<depth>`: `ROLLBACK TO SAVEPOINT` leaves the savepoint defined and
//! PostgreSQL resolves a name to the most recently established one, so a reused
//! name shadows an enclosing frame and sends its rollback to the wrong scope.
//!
//! ## Single-connection model & backend scope
//!
//! There is exactly one transaction connection slot per (app, isolate).
//! It lives in `ThreadDbContext::tx_conns` and a second top-level
//! transaction for the same app **waits** for it
//! ([`AwaitTxClaim`]) rather than racing for it.
//!
//! An earlier version of this paragraph said "V8 is single-threaded per
//! isolate, so only one transaction connection is active at a time",
//! which is why the slot was treated as safe to read ambiently. It does
//! not follow. Single-threaded means one *executing frame* at a time, not
//! one *in-flight operation*: a worker thread multiplexes many requests
//! over one isolate and hands the thread to another dispatch at every
//! `.await`, so two `transaction()` calls overlap routinely. The
//! serialisation above is what actually makes the one-slot model hold.
//!
//! Nested savepoints reuse the **same** connection (that is the whole
//! point of `SAVEPOINT`), so no new connection is acquired for a nested
//! `transaction()`.
//!
//! **Known gap.** Ordinary (non-transactional) CRUD still routes through
//! that slot ambiently: [`crate::exec::run_sql`] asks
//! `has_tx_for(app_id)` with no notion of *whose* transaction it is, so a
//! plain `env.db.x.insert()` issued while some unrelated unit of work
//! holds a transaction open executes inside it and is undone by its
//! rollback. Measured on both tiers (`cxPlain` in
//! `tests/e2e_dev_vs_deployed_db.sh`). Closing it means capturing the
//! async scope at each CRUD dispatch site the way this module now does
//! for `transaction()`.
//!
//! The top-level `BEGIN` path acquires a backend-specific dedicated
//! client via [`crate::backend::SqlExecutor::acquire_dedicated_client`]
//! and parks it in the per-isolate `tx_conn` slot. Postgres stores a
//! dedicated libpq connection; SQLite stores a handle to the shared
//! session actor and drives the same `BEGIN` / `SAVEPOINT` /
//! `COMMIT` / `ROLLBACK` verbs over that one connection. SQLite ignores
//! the SDK isolation-level hint (it has no `ISOLATION LEVEL` clause);
//! the successful-path semantics remain Tier-1 parity, while
//! concurrency/isolation nuance stays documented as a divergence.

#![allow(unsafe_code)]

/// The SC-1 transaction protocol reducer.
///
/// A pure state machine - nine states, one gate for every forcing publisher,
/// one deadline slot, one lifecycle classifier. It performs no I/O and owns no
/// session, which is what makes SC-1's invariants checkable without a database.
///
/// [`transaction_dispatch`] runs on it: every begin, frame open, frame close
/// and settlement below is an event applied to this machine, and the SQL that
/// results is whatever [`driver`] was told to issue.
pub mod reducer;

/// The driver: the only place a reducer action becomes I/O.
pub mod driver;

/// The out-of-band canceller forced cleanup uses to reach a session another
/// future is holding.
pub(crate) mod cancel;

/// The `test-helpers` seam onto the driver, for integration targets that need a
/// live server. Not compiled into a production build.
#[cfg(any(test, feature = "test-helpers"))]
pub mod probe;

use crate::op_error::ToOpError;
use std::cell::Cell;

use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};

use crate::backend::pg_error;
use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::DbError;
use crate::exec::clear_pending_emits;
use crate::tx_route::TxRoute;
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

/// Execute a control statement (`BEGIN`, `SAVEPOINT`, `RELEASE`,
/// `ROLLBACK TO`) against the backend-specific pinned tx client.
///
/// **Terminal statements do NOT come through here.** A terminal statement's
/// command tag is not cosmetic - PostgreSQL answers `COMMIT` with the tag
/// `ROLLBACK` when the transaction is in a failed state, and this path goes
/// through `client_exec`, which returns `Ok(rows.len())` and throws the tag
/// away. `driver::terminal` reads the tag and classifies the three-way
/// [`reducer::TerminalResult`] the state machine needs.
pub(crate) async fn client_exec_on_tx(
    backend: &crate::backend::BackendHandle,
    client: &crate::context::TxConnection,
    sql: &str,
    params: &[&str],
) -> Result<u64, zeroship_data_core::error::DbError> {
    use crate::backend::SqlExecutor;
    use crate::context::TxConnection;

    match (backend, client) {
        (crate::backend::BackendHandle::Postgres(pg), TxConnection::Postgres(client)) => {
            pg.client_exec(client, sql, params).await
        }
        (crate::backend::BackendHandle::Sqlite(sq), TxConnection::Sqlite(client)) => {
            sq.client_exec(client, sql, params).await
        }
        (crate::backend::BackendHandle::Postgres(_), TxConnection::Sqlite(_))
        | (crate::backend::BackendHandle::Sqlite(_), TxConnection::Postgres(_)) => Err(
            zeroship_data_core::error::DbError::internal("db: transaction backend/client mismatch"),
        ),
    }
}

/// Apply the §17.5 per-app PG role to a transaction's dedicated client.
///
/// Issues `SET LOCAL ROLE "<per-app role>"` on `client` so every
/// statement in the surrounding transaction executes under the
/// constrained per-app role rather than the platform login role. `SET
/// LOCAL` auto-reverts at COMMIT / ROLLBACK, so a pooled / dedicated
/// connection can never leak the role to a later use.
///
/// The per-app role is provisioned by the migration service. The WAL
/// consumer + §17.6 watchdog + §17.7 drop step 3 deliberately do NOT
/// call this — they stay on the platform role (the only connection
/// crossing the per-app trust boundary).
///
pub(crate) async fn apply_per_app_role(
    client: &compio_postgres::Client,
    app_id: &str,
) -> Result<(), zeroship_data_core::error::SessionSetupError> {
    // SET LOCAL ROLE + the DB-1 timeout guards (statement / idle-in-tx / lock)
    // in one simple-query batch — all SET LOCAL, so they revert at the tx end.
    // The idle-in-tx guard is the load-bearing defense: a creator callback that
    // never resolves can no longer pin this dedicated connection forever and
    // exhaust the shared Postgres for other tenants.
    let sql = crate::auth::bootstrap::tx_session_setup_sql(app_id)
        .map_err(zeroship_data_core::error::SessionSetupError::failed)?;
    client.simple_query(&sql).await.map_err(|e| {
        let mut classified = pg_error::classify_pg_per_app_session_setup(&e, app_id);
        zeroship_data_core::error::prefix_message(
            classified.error_mut(),
            "db: tx session setup (per-app section 17.5 + DB-1 guards): ",
        );
        classified
    })?;
    Ok(())
}

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
    binding: DbBinding,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id().to_string();
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
    // `tests/e2e_dev_vs_deployed_db.sh` (`cxOvl`).
    //
    // `current_tx_app` is true only inside the enclosing callback's own
    // continuation chain — see `crate::tx_scope`. SEC-1 still holds and is
    // now structural rather than incidental: a co-resident app's callback
    // plants ITS app_id, so the comparison below fails and this app opens
    // its own top-level BEGIN.
    let nested = crate::tx_scope::current_tx_app(scope).as_deref() == Some(app_id.as_str());

    // A nested call whose enclosing transaction has already settled (a
    // continuation that outlived its tx — e.g. a callback that was never
    // awaited) has nothing to open a SAVEPOINT on. Refuse loudly rather
    // than emitting SQL against a drained slot.
    if nested && !crate::context::with(|c| c.has_tx_for(&app_id)) {
        let err = DbError::validation_hinted(
            "transaction_scope_expired",
            "db.transaction: the enclosing transaction has already settled".to_string(),
            "A nested env.db.transaction(...) must run while its enclosing transaction is still \
             open; awaiting the outer transaction's result first makes this a top-level call.",
        );
        reject_outer_now(scope, &outer_global, err);
        return outer_promise;
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
        match exec_begin_or_savepoint(nested, isolation_level.as_deref(), &app_id).await {
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

/// Future that resolves once this app owns the top-level-transaction
/// claim (see [`crate::context::ThreadDbContext::try_claim_tx`]).
///
/// Held from before the `BEGIN` until after the `COMMIT`/`ROLLBACK` has
/// settled, so it covers the window in which the tx connection slot is
/// still empty — the window two same-turn `transaction()` calls both fell
/// into.
struct AwaitTxClaim {
    app_id: String,
}

impl AwaitTxClaim {
    fn new(app_id: String) -> Self {
        Self { app_id }
    }
}

impl std::future::Future for AwaitTxClaim {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if crate::context::with_mut(|c| c.try_claim_tx(&self.app_id)) {
            return std::task::Poll::Ready(());
        }
        // Lost. Park and re-check on the next release; `release_tx_claim`
        // wakes every waiter, so a spurious wake just re-runs this poll.
        crate::context::with_mut(|c| c.push_tx_waiter(&self.app_id, cx.waker().clone()));
        std::task::Poll::Pending
    }
}

/// The admission claim, as an RAII guard covering `Preparing` and `Starting`.
///
/// **SC-1 rule 5, the defect labelled DBR-11.** Cancellation before `BEGIN`
/// returns must not leak the claim, and that window is exactly these two states:
/// from the moment admission is granted to the moment the session is installed
/// and the reducer reaches `Idle`. Before this guard, a spawned op cancelled
/// inside that window left the claim held and every later transaction for the
/// app parked until the isolate was evicted.
///
/// It is disarmed by [`Self::handed_to_reducer`] and by nothing else. From
/// `Idle` onward the reducer emits `Action::ReleaseAdmission` on *every* path to
/// `Settled` - including the forced ones - so the release stops being something
/// a `return` can skip. A creator callback that never settles is bounded by the
/// execution deadline, which is armed on the same transition that granted
/// admission.
struct TxAdmission {
    app_id: String,
    armed: bool,
}

impl TxAdmission {
    /// Wait for the claim, then arm.
    async fn acquire(app_id: String) -> Self {
        AwaitTxClaim::new(app_id.clone()).await;
        Self {
            app_id,
            armed: true,
        }
    }

    /// The reducer owns the release from here on.
    fn handed_to_reducer(mut self) {
        self.armed = false;
    }
}

impl Drop for TxAdmission {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // This transaction is not going to settle. Retire its state, WITHDRAW
        // any session that was installed - a session abandoned mid-`BEGIN` has
        // unknown health and must not go back to the pool - and release the
        // claim so later transactions for this app are not parked forever.
        //
        // `withdraw_tx_session`, not `take_tx_client_for`: the session may be
        // out on loan behind a `TxClientSlotGuard` whose future was cancelled by
        // the same drop that got us here, and the tombstone is what destroys it
        // when that guard restores it.
        let client = crate::context::with_mut(|c| {
            c.retire_transaction(&self.app_id);
            let client = c.withdraw_tx_session(&self.app_id);
            c.release_tx_claim(&self.app_id);
            client
        });
        if let Some(client) = client {
            crate::context::destroy_tx_connection(client);
        }
        clear_pending_emits(&self.app_id);
    }
}

/// Transaction frame used by a native write that expands one creator call
/// into multiple SQL mutations.
///
/// A normally settled call outside `db.transaction()` owns a top-level
/// `BEGIN`/`COMMIT`; a call inside one owns a savepoint. A row failure is
/// returned only after that frame reports a successful rollback. Settlement
/// goes through [`exec_settle`], and therefore through the reducer - including
/// the PostgreSQL `COMMIT`-answered-with-`ROLLBACK` check the driver's terminal
/// classifier makes. Dropping an unsettled top-level frame withdraws its
/// session through [`TxAdmission`]: the connection is destroyed rather than
/// pooled, because a transaction abandoned mid-flight has unknown health.
#[must_use = "an atomic write frame must be settled with finish"]
pub(crate) struct AtomicWriteFrame {
    route: TxRoute,
    frame: Option<reducer::frames::FrameId>,
    state: AtomicWriteFrameState,
    /// Held for a top-level frame until the settle path takes over, so a
    /// cancelled `begin` releases the claim it took. See [`TxAdmission`].
    admission: Option<TxAdmission>,
}

#[derive(Clone, Copy)]
enum AtomicWriteFrameState {
    Open,
    Settling,
    Settled,
}

impl AtomicWriteFrame {
    /// Open the frame and promote the captured dispatch route onto it.
    pub(crate) async fn begin(route: TxRoute) -> Result<Self, DbError> {
        let nested = route.in_tx();
        let app_id = route.app_id().to_string();
        if nested && !crate::context::with(|context| context.has_tx_for(&app_id)) {
            return Err(DbError::validation_hinted(
                "transaction_scope_expired",
                "updateMany: the enclosing transaction has already settled".to_string(),
                "Run updateMany while its enclosing db.transaction callback is still open.",
            ));
        }
        // The depth cap is the frame stack's, not a second copy here.
        let admission = if nested {
            None
        } else {
            Some(TxAdmission::acquire(app_id.clone()).await)
        };

        match exec_begin_or_savepoint(nested, None, &app_id).await {
            Ok(frame) => Ok(Self {
                route: route.into_internal_transaction(),
                frame,
                state: AtomicWriteFrameState::Open,
                admission,
            }),
            // `admission` drops here, releasing the claim and destroying any
            // session that was installed.
            Err(error) => Err(error),
        }
    }

    /// Route all statements and broker effects through this frame.
    pub(crate) fn route(&self) -> &TxRoute {
        &self.route
    }

    /// Commit/release a successful body or roll back a failed one.
    ///
    /// The body value is returned only after a confirmed successful settle.
    /// On rollback, the original row error remains the creator-visible error;
    /// a savepoint settle failure wins because the enclosing transaction state
    /// is then no longer trustworthy.
    pub(crate) async fn finish<T>(mut self, body: Result<T, DbError>) -> Result<T, DbError> {
        let success = body.is_ok();
        // The reducer owns the admission release from here: every path it takes
        // to `Settled` emits `ReleaseAdmission`.
        if let Some(admission) = self.admission.take() {
            admission.handed_to_reducer();
        }
        self.state = AtomicWriteFrameState::Settling;
        let outcome = exec_settle(self.route.app_id(), success, self.frame).await;
        self.state = AtomicWriteFrameState::Settled;
        match (body, outcome) {
            (Ok(value), SettleOutcome::Ok) => Ok(value),
            (Err(error), SettleOutcome::Ok) => Err(error),
            (_, SettleOutcome::CommitIndeterminate(error)) => {
                Err(commit_failed_indeterminate(error))
            }
            (_, SettleOutcome::SettleErr(error)) => Err(error),
        }
    }
}

impl Drop for AtomicWriteFrame {
    fn drop(&mut self) {
        if !matches!(self.state, AtomicWriteFrameState::Open) {
            return;
        }

        let app_id = self.route.app_id();
        if self.frame.is_some() {
            // A nested frame cannot synchronously issue ROLLBACK TO from Drop.
            // The enclosing transaction still owns the session and must settle
            // it; do not release its claim or destroy its session here.
            tracing::warn!(
                app_id,
                "nested atomic write frame dropped before savepoint settlement"
            );
            return;
        }

        // A top-level frame dropped unsettled is the cancellation case, and
        // `admission`'s own Drop is what handles it: retire the reducer,
        // destroy the session rather than pool it, release the claim, clear the
        // queued events. Nothing is open-coded here any more, so the two paths
        // cannot disagree about what a cancelled admission owes.
        //
        // The SQLite fail-closed arm this replaced ("no client; retaining
        // claim") kept a claim forever whenever the exec path happened to hold
        // the handle at drop time. The withdrawal tombstone answers that
        // properly: the handle IS destroyed, when its holder returns it.
        drop(self.admission.take());
    }
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

/// Run `BEGIN [ISOLATION LEVEL ...]` (top-level) or `SAVEPOINT <frame>`
/// (nested), through the reducer.
///
/// Returns `Ok(None)` for a top-level begin, `Ok(Some(frame))` for the frame a
/// nested begin opened. The savepoint NAME never leaves the frame stack: the
/// caller settles by frame id, and the name the settle emits is the one that
/// frame minted.
async fn exec_begin_or_savepoint(
    nested: bool,
    isolation_level: Option<&str>,
    app_id: &str,
) -> Result<Option<reducer::frames::FrameId>, DbError> {
    if nested {
        let driven = driver::open_frame(app_id).await;
        if let Some(refusal) = driven.refusal() {
            return Err(frame_refusal(refusal, driven.error));
        }
        let frame = driven.frame().ok_or_else(|| {
            driven.error.unwrap_or_else(|| {
                DbError::internal("db.transaction: SAVEPOINT did not open a frame")
            })
        })?;
        return Ok(Some(frame));
    }

    let driven = driver::begin_top_level(app_id, isolation_level).await?;
    let outcome = driven.outcome();
    match outcome {
        // Only the genuinely unclassified startup outcome gets the generic
        // code. SetupFailed, ReResolve, and Denied all carry the exact typed
        // DbError captured beside their BeginOutcome.
        Some(
            reducer::TerminalOutcome::Cancelled(reducer::CleanupCause::BeginFailed)
            | reducer::TerminalOutcome::Indeterminate(reducer::CleanupCause::BeginFailed),
        ) => {
            let error = driven.error.unwrap_or_else(|| {
                DbError::internal("db.transaction: BEGIN did not open a transaction")
            });
            Err(DbError::Coded {
                code: "begin_failed".to_string(),
                message: format!("db.transaction: BEGIN failed: {}", error.message_str()),
                hint: None,
            })
        }
        Some(_) => Err(driven.error.unwrap_or_else(|| {
            DbError::internal("db.transaction: classified BEGIN failure carried no detail")
        })),
        None => match driven.refusal() {
            Some(refusal) => Err(driver::protocol_error(refusal, driven.error)),
            None => Ok(None),
        },
    }
}

/// Lower a frame guard's refusal to the creator-visible error.
///
/// `savepoint_depth_exceeded` keeps the hinted validation shape it has always
/// had; the frame stack is where the cap now lives, so this is the only place
/// that spells it.
fn frame_refusal(refusal: reducer::TxProtocolError, detail: Option<DbError>) -> DbError {
    if refusal
        == reducer::TxProtocolError::Frame(reducer::frames::FrameError::SavepointDepthExceeded)
    {
        return DbError::validation_hinted(
            "savepoint_depth_exceeded",
            format!(
                "db.transaction: nested transaction depth limit ({MAX_SAVEPOINT_DEPTH}) \
                 exceeded — flatten the nesting or split the work into separate transactions"
            ),
            "Each nested env.db.transaction(...) opens a SAVEPOINT; the cap guards against \
             runaway recursion.",
        );
    }
    driver::protocol_error(refusal, detail)
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

/// Run one creator data statement inside `app_id`'s open transaction.
///
/// Goes through the reducer's operation guard: the statement takes the session,
/// and its outcome is reported back. A statement that errors leaves the
/// transaction in `Poisoned`, which is where PostgreSQL has already put it -
/// every further data statement answers `25P02` until the block ends.
///
/// **The savepoint statements no longer come through here.** They are frame
/// events now, and the name they carry is the frame's.
///
/// See [`driver::run_operation`] for why the creator CRUD path has not moved
/// onto this yet.
#[allow(
    dead_code,
    reason = "the tests below are its only callers until exec.rs's in-transaction \
              arms move onto the reducer's operation guard"
)]
pub(crate) async fn run_on_tx_conn(app_id: &str, sql: &str) -> Result<(), DbError> {
    driver::run_operation(app_id, sql, &[]).await
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
    let prev_scope = crate::tx_scope::enter(scope, app_id);
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
                || zeroship_data_core::error::DbError::internal("db.transaction: callback threw").to_op_error(),
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

// ---------------------------------------------------------------------------
// exec_settle — COMMIT / ROLLBACK / RELEASE / ROLLBACK TO
// ---------------------------------------------------------------------------

/// Result of the settle SQL, carrying enough to build the outer promise's
/// `ResolveValue`.
#[derive(Debug)]
enum SettleOutcome {
    /// Settle SQL succeeded.
    Ok,
    /// COMMIT failed after the body resolved — the tx state is
    /// indeterminate; reject with `commit_failed_indeterminate`.
    CommitIndeterminate(DbError),
    /// A rollback / release / rollback-to failed. The frame state is no
    /// longer trustworthy, so this error governs how the outer settles.
    SettleErr(DbError),
}

/// Run the settle for this transaction level, through the reducer.
///
/// Top-level (`frame == None`):
///   - success → `COMMIT`, dispose of the session, publish the queued effects.
///   - failure → `ROLLBACK`, dispose of the session, discard them.
///
/// Nested (`frame == Some(id)`):
///   - success → `RELEASE` (keeps the session open).
///   - failure → `ROLLBACK TO` **then** `RELEASE` (keeps it open; the enclosing
///     transaction continues). The second statement is not optional: `ROLLBACK
///     TO SAVEPOINT` leaves the savepoint defined, and leaving it there is what
///     a later frame of the same name would shadow.
///
/// **DBR-03 is gone from this function.** It used to read "slot already drained
/// (e.g. a concurrent teardown). Treat as settled" and return
/// `SettleOutcome::Ok` **without sending anything** - so an absent client was
/// proof that terminal SQL had run. Under the reducer the only state that ends a
/// settle early is `Settled`; a settle that arrives while an operation owns the
/// session waits in `Quiescing` for it to come back, and a session that is
/// genuinely unreachable when terminal SQL is due is `Indeterminate`, which
/// withdraws and tells the creator.
async fn exec_settle(
    app_id: &str,
    success: bool,
    frame: Option<reducer::frames::FrameId>,
) -> SettleOutcome {
    match frame {
        Some(frame) => {
            let close = if success {
                reducer::frames::FrameClose::Released
            } else {
                reducer::frames::FrameClose::RolledBackTo
            };
            let driven = driver::close_frame(app_id, frame, close).await;
            if driven.frame().is_some() {
                return SettleOutcome::Ok;
            }
            // A FAILED frame settle keeps the frame's events. The transaction
            // is poisoned and will not commit (a COMMIT on a failed tx is
            // answered `ROLLBACK`, which the driver's terminal classifier
            // detects), so nothing publishes either way - but the events are
            // the diagnostic evidence for why the close failed, and discarding
            // them before the statement ran destroyed exactly that.
            let refusal = driven.refusal();
            let error = driven
                .error
                .or_else(|| refusal.map(|r| driver::protocol_error(r, None)))
                .unwrap_or_else(|| DbError::internal("db.transaction: savepoint settle failed"));
            SettleOutcome::SettleErr(if success {
                savepoint_release_failed_indeterminate(error)
            } else {
                rollback_failed_indeterminate(error)
            })
        }
        None => {
            let intent = if success {
                reducer::SettleIntent::Commit
            } else {
                reducer::SettleIntent::Rollback
            };
            let driven = driver::settle_root(app_id, intent).await;
            let Some(outcome) = driven.outcome() else {
                let error = driven.refusal().map_or_else(
                    || DbError::internal("db.transaction: the settle produced no outcome"),
                    |refusal| driver::protocol_error(refusal, None),
                );
                return SettleOutcome::SettleErr(error);
            };
            // The driver's mapping already carries the RIGHT code for each
            // outcome, so nothing here re-wraps it. Re-wrapping is how a
            // definitively rolled-back commit came out labelled
            // `commit_failed_indeterminate`: the outcome was known, not unknown,
            // and the label said the opposite of what the state machine
            // established.
            match driver::outcome_error(outcome, intent, driven.error) {
                None => SettleOutcome::Ok,
                Some(error) if matches!(outcome, reducer::TerminalOutcome::Indeterminate(_)) => {
                    SettleOutcome::CommitIndeterminate(error)
                }
                Some(error) => SettleOutcome::SettleErr(error),
            }
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

fn commit_failed_indeterminate(error: DbError) -> DbError {
    DbError::Coded {
        code: "commit_failed_indeterminate".to_string(),
        message: format!(
            "commit failed - transaction state indeterminate: {}",
            error.message_str()
        ),
        hint: None,
    }
}

fn rollback_failed_indeterminate(error: DbError) -> DbError {
    DbError::Coded {
        code: "rollback_failed_indeterminate".to_string(),
        message: format!(
            "rollback failed - transaction state indeterminate: {}",
            error.message_str()
        ),
        hint: None,
    }
}

fn savepoint_release_failed_indeterminate(error: DbError) -> DbError {
    DbError::Coded {
        code: "savepoint_release_failed_indeterminate".to_string(),
        message: format!(
            "savepoint release failed - transaction state indeterminate: {}",
            error.message_str()
        ),
        hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::rc::Rc;

    use crate::backend::SqlExecutor;
    use crate::backend::sqlite::SqliteBackend;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    struct ContextReset;

    impl Drop for ContextReset {
        fn drop(&mut self) {
            crate::context::with_mut(|c| {
                let _ = c.take_tx_client_for("app_sqlite");
                c.retire_transaction("app_sqlite");
                c.release_tx_claim("app_sqlite");
                c.clear_pending_emits_for("app_sqlite");
                c.clear_pool();
            });
        }
    }

    fn install_sqlite_backend_for_test() -> (Rc<SqliteBackend>, tempfile::TempDir, ContextReset) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let backend = Rc::new(
            crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()))
                .expect("open sqlite backend"),
        );
        let reset = ContextReset;
        crate::context::with_mut(|c| {
            let _ = c.take_tx_client_for("app_sqlite");
            c.retire_transaction("app_sqlite");
            c.release_tx_claim("app_sqlite");
            c.clear_pending_emits_for("app_sqlite");
            c.set_sqlite_backend(Rc::clone(&backend));
        });
        (backend, dir, reset)
    }

    /// **Dispatch emits the reducer's monotonic names, not depth-derived ones.**
    ///
    /// This replaced `savepoint_name_is_prefixed_and_1_based`, which asserted
    /// `savepoint_name(1) == "zs_sp_1"` - a function whose whole contract was
    /// the defect. Two frames opened at the SAME depth used to get the same
    /// name, and because `ROLLBACK TO SAVEPOINT` leaves the savepoint defined
    /// and PostgreSQL resolves a name to the most recently established one, the
    /// leftover shadowed the enclosing frame and sent its rollback to the wrong
    /// scope.
    ///
    /// The arm drives the real dispatch entry point, not the frame stack: it
    /// opens a frame, settles it, opens another at the same depth, and requires
    /// the two minted names to differ. Mutating `FrameStack::open_child` back to
    /// a depth-derived `format!("zs_sp_{}", self.frames.len())` reddens this.
    #[test]
    fn dispatch_emits_monotonic_savepoint_names_at_the_same_depth() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .client_exec(&probe, "CREATE TABLE notes (id INTEGER PRIMARY KEY)", &[])
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite")
                .await
                .expect("begin");

            let first = exec_begin_or_savepoint(true, None, "app_sqlite")
                .await
                .expect("first nested frame")
                .expect("a nested begin opens a frame");
            // Roll it back, which on PostgreSQL leaves the savepoint defined -
            // the precondition that makes a reused name dangerous.
            match exec_settle("app_sqlite", false, Some(first)).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok for the first frame settle, got {other:?}"),
            }

            let second = exec_begin_or_savepoint(true, None, "app_sqlite")
                .await
                .expect("second nested frame")
                .expect("a nested begin opens a frame");
            assert_ne!(
                first, second,
                "a frame id is minted from a monotonic sequence and never reused"
            );

            let names = crate::context::with(|c| {
                c.transaction_reducer("app_sqlite")
                    .expect("the transaction is still open")
                    .frames()
                    .minted_names()
                    .clone()
            });
            assert_eq!(
                names.len(),
                2,
                "two frames at the same depth must mint two DISTINCT names; a \
                 depth-derived scheme mints one name twice and the set collapses \
                 to a single entry. got {names:?}"
            );
            assert!(
                names.iter().all(|name| name.starts_with("zs_sp_")),
                "the zs_ prefix keeps the name out of any plausible user-chosen \
                 savepoint namespace; got {names:?}"
            );

            match exec_settle("app_sqlite", false, Some(second)).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok for the second frame settle, got {other:?}"),
            }
            let _ = exec_settle("app_sqlite", false, None).await;
        });
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

    /// The settle's own code reaches the creator, UNWRAPPED.
    ///
    /// It used to be re-wrapped in `commit_failed_indeterminate` here, which
    /// labelled every failing settle "indeterminate" - including a COMMIT the
    /// server answered `ROLLBACK`, whose outcome is not unknown at all. The
    /// codes now come from `driver::outcome_error`, one per outcome.
    #[test]
    fn a_settles_own_code_reaches_the_creator_unwrapped() {
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
            match build_settle_resolve_value(outcome, true, None) {
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

    /// `driver::outcome_error` names each outcome distinctly.
    ///
    /// A commit the server rolled back is NOT indeterminate: its outcome is
    /// known and its writes are gone. Collapsing the two hides a definite
    /// failure behind a retryable-looking one.
    #[test]
    fn outcome_errors_do_not_collapse_rolled_back_into_indeterminate() {
        use reducer::{CleanupCause, SettleIntent, TerminalOutcome};

        fn code_of(error: &DbError) -> &str {
            match error {
                DbError::Coded { code, .. } => code,
                other => panic!("expected a coded error, got {other:?}"),
            }
        }

        assert!(
            driver::outcome_error(TerminalOutcome::Committed, SettleIntent::Commit, None).is_none(),
            "a confirmed commit is not an error"
        );
        let rolled_back =
            driver::outcome_error(TerminalOutcome::RolledBack, SettleIntent::Commit, None)
                .expect("a COMMIT answered ROLLBACK is a failure");
        let indeterminate = driver::outcome_error(
            TerminalOutcome::Indeterminate(CleanupCause::BackendHealthUnknown),
            SettleIntent::Commit,
            None,
        )
        .expect("an unproved terminal is a failure");
        assert_eq!(code_of(&rolled_back), "commit_rolled_back");
        assert_eq!(code_of(&indeterminate), "commit_failed_indeterminate");
        assert_ne!(
            code_of(&rolled_back),
            code_of(&indeterminate),
            "a definite failure must not be reported under the code for an \
             unknown one - the observable difference would migrate into retry \
             timing, which every caller can measure and none can act on"
        );

        // Every cleanup cause reaches the creator under its OWN code, so a
        // caller can tell a deadline from a denial from a cancel.
        for cause in [
            CleanupCause::Cancelled,
            CleanupCause::Detached,
            CleanupCause::EpochChanged,
            CleanupCause::BeginFailed,
        ] {
            let error = driver::outcome_error(
                TerminalOutcome::Cancelled(cause),
                SettleIntent::Commit,
                None,
            )
            .expect("a cancelled transaction is an error to the caller");
            assert_eq!(code_of(&error), cause.code());
        }
    }

    #[test]
    fn build_settle_resolve_value_ok_resolve_undefined_when_no_body() {
        let rv = build_settle_resolve_value(SettleOutcome::Ok, true, None);
        matches!(rv, ResolveValue::Undefined)
            .then_some(())
            .expect("expected Undefined for ok+no-body");
    }

    /// The probe reads and seeds through `op_conn`; writes that belong to the
    /// transaction go through [`run_on_tx_conn`], which is the only path onto
    /// `tx_conn`.
    ///
    /// Before SC-2 these tests used `acquire_dedicated_client()` for the probe
    /// AND drove the transaction's writes through it, which worked only
    /// because both were the same single connection. That coupling is the
    /// divergence SC-2 retires, so the probe now names the lane it wants.
    #[test]
    fn sqlite_top_level_begin_ignores_isolation_and_commits() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .client_exec(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, Some("SERIALIZABLE"), "app_sqlite")
                .await
                .expect("begin sqlite tx");
            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('kept')")
                .await
                .expect("insert inside sqlite tx");

            match exec_settle("app_sqlite", true, None).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok settle outcome, got {other:?}"),
            }

            let rows = probe
                .query_internal("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after commit");
            assert_eq!(rows[0][0].as_deref(), Some("1"));
            assert!(!crate::context::with(|c| c.has_tx_for("app_sqlite")));
        });
    }

    #[test]
    fn sqlite_top_level_reject_path_actively_rolls_back() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .client_exec(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite")
                .await
                .expect("begin sqlite tx");
            run_on_tx_conn(
                "app_sqlite",
                "INSERT INTO notes (title) VALUES ('rolled-back')",
            )
            .await
            .expect("insert inside sqlite tx");

            match exec_settle("app_sqlite", false, None).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok settle outcome, got {other:?}"),
            }

            let rows = probe
                .query_internal("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after rollback");
            assert_eq!(rows[0][0].as_deref(), Some("0"));
            assert!(!crate::context::with(|c| c.has_tx_for("app_sqlite")));
        });
    }

    /// **Forced cleanup reaches an unreachable SQLite session through SC-2's
    /// canceller, and rolls it back rather than giving up on it.**
    ///
    /// The PostgreSQL arm of this property is
    /// `a_forced_cleanup_cancels_the_running_statement_and_keeps_the_connection`
    /// in `tests/native_transaction.rs`; this is its dev-tier peer, and it is
    /// here rather than there because it needs no server.
    ///
    /// The two backends differ in how much a cancellation accomplishes, and this
    /// arm exists to hold that difference to the assertion rather than to a
    /// comment: SQLite's actor answers a `Cancel` only after it has rolled back
    /// and retired the reservation, so there is nothing to reclaim and no
    /// `ROLLBACK` for the driver to issue afterwards. `SqliteCancelHandle::cancel`
    /// carried a `// no production canceller yet` allow until this change; this
    /// is the caller that made it false.
    ///
    /// The session is taken out of the slot with a bare
    /// [`crate::context::TxClientSlotGuard`], which is what ordinary CRUD does
    /// (the module header's "known gap"), so the reducer still reports
    /// `SessionOwnership::Registry`. That is the production shape today, not a
    /// contrived one.
    ///
    /// **The row count is the load-bearing assertion.** An outcome of
    /// `Cancelled` only means the driver believed a rollback happened; the
    /// probe reading `0` on a SEPARATE connection is the actor having actually
    /// done it.
    ///
    /// **Mutation that reddens this arm:** in `driver::cleanup`, answer the
    /// empty-slot `Registry`/`Command` case with `CleanupAck::Indeterminate`
    /// instead of `cancel_and_reclaim(..)`. The outcome becomes
    /// `Indeterminate(Cancelled)` and the session is withdrawn.
    #[test]
    fn a_forced_cleanup_cancels_an_unreachable_sqlite_session() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .client_exec(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite")
                .await
                .expect("begin sqlite tx");
            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('doomed')")
                .await
                .expect("insert inside sqlite tx");

            // Another future owns the session. Forced cleanup cannot take it out
            // of the slot, so the only route left is the canceller captured when
            // the session was installed.
            let held =
                crate::context::TxClientSlotGuard::take("app_sqlite").expect("hold the session");

            let driven = driver::cancel("app_sqlite").await;
            assert_eq!(
                driven.outcome(),
                Some(reducer::TerminalOutcome::Cancelled(
                    reducer::CleanupCause::Cancelled
                )),
                "the actor rolls back and retires the reservation before it \
                 acknowledges, so the cleanup goal is PROVED - answering \
                 Indeterminate here is what used to abandon a live session"
            );
            assert!(
                !crate::context::with(|c| c.tx_session_withdrawn("app_sqlite")),
                "a proved cleanup withdraws nothing"
            );

            drop(held);

            let rows = probe
                .query_internal("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after the forced cleanup");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("0"),
                "the cancellation must have really rolled the transaction back, \
                 not merely reported that it did"
            );
        });
    }

    /// **The divergence SC-2 Decision 1 retires**, stated as an assertion.
    ///
    /// `tx_route.rs` used to carry: "SQLite runs the whole app on one
    /// connection ... so a correctly pool-routed write still executes inside
    /// whatever transaction that connection is holding." That is what this
    /// test measures: an autocommit write issued while the app's OWN explicit
    /// transaction is open, followed by that transaction's `ROLLBACK`.
    ///
    /// On one connection the autocommit row is destroyed - the pre-SC-2 tree
    /// leaves `0` rows. With `op_conn` and `tx_conn` split it survives and the
    /// transaction's own row does not.
    ///
    /// It says autocommit **write** and issues it before the transaction takes
    /// the write lock, deliberately. WAL gives concurrent readers, not
    /// concurrent writers: an autocommit write racing a `tx_conn` that already
    /// holds the write lock still waits out `busy_timeout`, and no number of
    /// connections changes that.
    #[test]
    fn an_autocommit_write_survives_the_apps_own_transaction_rollback() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .client_exec(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite")
                .await
                .expect("begin sqlite tx");

            // No transaction anywhere in this call's async scope.
            backend
                .client_exec(
                    &probe,
                    "INSERT INTO notes (title) VALUES ('autocommit')",
                    &[],
                )
                .await
                .expect("autocommit insert while a transaction is open");

            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('doomed')")
                .await
                .expect("insert inside sqlite tx");

            match exec_settle("app_sqlite", false, None).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok settle outcome, got {other:?}"),
            }

            let rows = probe
                .query_internal("SELECT title FROM notes ORDER BY id", &[])
                .await
                .expect("read notes after rollback");
            assert_eq!(
                rows.len(),
                1,
                "the autocommit write must survive the transaction's ROLLBACK \
                 and the transaction's own write must not; got {rows:?}"
            );
            assert_eq!(rows[0][0].as_deref(), Some("autocommit"));
        });
    }

    #[test]
    fn dropped_atomic_write_frame_rolls_back_top_level_sqlite() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .client_exec(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            let frame = AtomicWriteFrame::begin(TxRoute::pool_for_tests("app_sqlite"))
                .await
                .expect("begin atomic write frame");
            run_on_tx_conn(
                "app_sqlite",
                "INSERT INTO notes (title) VALUES ('must-rollback')",
            )
            .await
            .expect("insert inside atomic write frame");
            drop(frame);

            let rows = probe
                .query_internal("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after frame cancellation");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("0"),
                "dropping an unsettled frame must roll back its writes"
            );
            assert!(!crate::context::with(|context| {
                context.has_tx_for("app_sqlite") || context.tx_claimed_by("app_sqlite")
            }));
        });
    }
}
