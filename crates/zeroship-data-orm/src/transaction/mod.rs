//! Native `Db.transaction(asyncFn, opts?)` orchestrator.
//!
//! Transaction orchestration lives **entirely in Rust**.
//! The creator API is unchanged — `await env.db.transaction(async tx =>
//! {...})` commits on resolve, rolls back on throw — but the begin /
//! commit / rollback / nested-savepoint state machine moved out of the
//! bootstrap's JS `transactionImpl` and into `transaction_dispatch`.
//! `db.beginTransaction()` and the `Transaction` v8_class methods
//! (`commit`/`rollback`/`collection`) no longer exist on the JS surface.
//!
//! ## The orchestrator runs on the SC-1 reducer
//!
//! **This module owns the protocol and NOT the V8 shape, which is the reverse
//! of what this paragraph said until 2026-09-02.** The promises, continuations
//! and `.then` handlers moved to the adapter tier's `v8_classes::transaction`; eleven
//! items went, picked by whether their signature names a `v8::` type. What
//! stays here is the state machine and the I/O it drives: every transition is
//! an event applied to [`reducer::TxReducer`], and every statement reaching the
//! wire is an [`reducer::Action`] [`driver`] was told to issue. There is no
//! second copy of the rules: the depth cap, the savepoint names, the effect
//! fate, the session disposition and the admission release all live in the
//! state machine.
//!
//! The eight-step flow below therefore spans two files. Steps 1, 4, 5, 6 and
//! the two handlers are over in the adapter; steps 2, 3, 7 and 8 resolve here,
//! through `exec_begin_or_savepoint` and `exec_settle`. The data crossing
//! between them is an app id, a [`reducer::frames::FrameId`] and a
//! [`SettleOutcome`] - never a scope, never a resolver.
//!
//! 1. `transaction_dispatch` (a sync v8_method body) mints the outer
//!    `v8::PromiseResolver` and returns its promise to JS immediately.
//! 2. It reads the calling frame's **async context**
//!    (the adapter tier's `tx_scope`) to decide whether this is a **top-level**
//!    transaction (not inside any transaction callback → admit a reducer and
//!    emit `BEGIN`) or a **nested** one (inside this app's enclosing callback →
//!    open a frame and emit `SAVEPOINT`). It is deliberately NOT "does this app
//!    have a transaction open right now" — that test cannot tell a nested call
//!    from an unrelated concurrent one, and reading it that way silently folded
//!    one request's transaction into another's (see that module for the
//!    measurement).
//! 3. A spawned op takes the admission claim as an RAII [`TxAdmission`] guard
//!    and runs the `BEGIN` / `SAVEPOINT` through the reducer. On success it
//!    hands back a `zeroship_runtime::state::ResolveValue::Continuation`; on
//!    failure the guard's drop releases the claim, retires the reducer and
//!    withdraws any session that was installed.
//! 4. The continuation runs inside the pump's V8 scope:
//!    `v8_classes::transaction::mint_tx_view`
//!    builds the tx-view object (collections-as-props, no
//!    commit/rollback methods), then the creator callback is invoked
//!    inside a `v8::TryCatch` to capture a synchronous throw.
//! 5. The callback's return is coerced to a Promise
//!    (`coerce_to_promise`): an already-Promise is used as-is; a plain
//!    value is wrapped resolved; a synchronous throw skips straight to
//!    the rollback path.
//! 6. `.then(resolve_handler, reject_handler)` is attached to that
//!    Promise. The handlers are native `v8::Function`s whose `.data()`
//!    carries a heap `TxFinalizer` (the outer resolver + the FRAME ID + the
//!    request id — never a savepoint name).
//! 7. On the creator promise **resolving**, `tx_resolve_handler` settles:
//!    `COMMIT` at the root, `RELEASE` for a frame.
//! 8. On the creator promise **rejecting**, `tx_reject_handler` settles:
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
//! `examples/db-todos/tests/database.test.ts`). Closing it means capturing the
//! async scope at each CRUD dispatch site the way this module now does
//! for `transaction()`.
//!
//! The top-level `BEGIN` path acquires a backend-specific dedicated
//! client via [`crate::backend::DatabaseFixture::fixture_session`]
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
/// The adapter tier's `transaction_dispatch` runs on it: every begin, frame open, frame close
/// and settlement below is an event applied to this machine, and the SQL that
/// results is whatever [`driver`] was told to issue.
pub mod reducer;
pub mod scope;

/// The driver: the only place a reducer action becomes I/O.
pub mod driver;

// The canceller's doc comment used to sit here, above a `pub mod cancel;` that
// #122 moved to `backend/cancel.rs` - the declaration went, the documentation
// stayed, and rustdoc then attached it to whatever came next. Deleted rather
// than re-pointed: `backend/mod.rs:80` declares the module and carries its own.

/// The `test-helpers` seam onto the driver, for integration targets that need a
/// live server. Not compiled into a production build.
#[cfg(any(test, feature = "test-helpers"))]
pub mod probe;

use crate::exec::clear_pending_emits;
use crate::tx_route::TxRoute;
use zeroship_data_orm::error::DbError;

/// Maximum nesting depth for `env.db.transaction(...)` calls — the
/// outermost `BEGIN` plus this many `SAVEPOINT` levels. A `transaction()`
/// call that would open the `(MAX_SAVEPOINT_DEPTH + 1)`-th savepoint
/// rejects with `savepoint_depth_exceeded`.
///
/// 8 matches the proposal's depth cap (Q-P9-D). Real code rarely nests
/// transactions beyond two or three levels; the cap is a runaway-recursion
/// guard, not a workload limit.
pub const MAX_SAVEPOINT_DEPTH: u32 = 8;

// ---------------------------------------------------------------------------
// TxFinalizer — heap state shared by the resolve / reject handlers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// transaction_dispatch — the v8_method entry point
// ---------------------------------------------------------------------------

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
        if crate::tx_lanes::with_mut(|l| l.try_claim_tx(&self.app_id)) {
            return std::task::Poll::Ready(());
        }
        // Lost. Park and re-check on the next release; `release_tx_claim`
        // wakes every waiter, so a spurious wake just re-runs this poll.
        crate::tx_lanes::with_mut(|l| l.push_tx_waiter(&self.app_id, cx.waker().clone()));
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
#[derive(Debug)]
pub struct TxAdmission {
    app_id: String,
    armed: bool,
}

impl TxAdmission {
    /// Wait for the claim, then arm.
    pub async fn acquire(app_id: String) -> Self {
        AwaitTxClaim::new(app_id.clone()).await;
        Self {
            app_id,
            armed: true,
        }
    }

    /// The reducer owns the release from here on.
    pub fn handed_to_reducer(mut self) {
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
        let client = crate::tx_lanes::with_mut(|l| {
            l.retire_transaction(&self.app_id);
            let client = l.withdraw_tx_session(&self.app_id);
            l.release_tx_claim(&self.app_id);
            client
        });
        if let Some(client) = client {
            crate::tx_lanes::destroy_tx_connection(client);
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
#[derive(Debug)]
pub struct AtomicWriteFrame {
    route: TxRoute,
    frame: Option<reducer::frames::FrameId>,
    state: AtomicWriteFrameState,
    /// Held for a top-level frame until the settle path takes over, so a
    /// cancelled `begin` releases the claim it took. See [`TxAdmission`].
    admission: Option<TxAdmission>,
}

#[derive(Debug, Clone, Copy)]
enum AtomicWriteFrameState {
    Open,
    Settling,
    Settled,
}

impl AtomicWriteFrame {
    /// Open the frame and promote the captured dispatch route onto it.
    pub async fn begin(route: TxRoute) -> Result<Self, DbError> {
        route.check_scope()?;
        let nested = route.in_tx();
        let app_id = route.app_id().to_string();
        let schema = route.schema().clone();
        if nested && !crate::tx_lanes::with(|l| l.has_tx_for(&app_id)) {
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

        // Cloned off the route while it is still alive - it is consumed by
        // `into_internal_transaction` two lines below. Both `BackendHandle`
        // arms are an `Rc`, so this is a refcount bump, not a second backend.
        let backend = route.backend().clone();
        match exec_begin_or_savepoint(nested, None, &app_id, schema, backend).await {
            Ok(frame) => Ok(Self {
                route: route.into_internal_transaction()?,
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
    pub fn route(&self) -> &TxRoute {
        &self.route
    }

    /// Commit/release a successful body or roll back a failed one.
    ///
    /// The body value is returned only after a confirmed successful settle.
    /// On rollback, the original row error remains the creator-visible error;
    /// a savepoint settle failure wins because the enclosing transaction state
    /// is then no longer trustworthy.
    pub async fn finish<T>(mut self, body: Result<T, DbError>) -> Result<T, DbError> {
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
///
/// `backend` is consumed only by the top-level arm - a SAVEPOINT runs on the
/// session the enclosing BEGIN already opened - but it is taken unconditionally
/// so the caller cannot be in the position of deciding whether one is needed.
pub async fn exec_begin_or_savepoint(
    nested: bool,
    isolation_level: Option<zeroship_data_orm::error::IsolationLevel>,
    app_id: &str,
    schema: zeroship_data_sql::SchemaName,
    backend: crate::backend::BackendHandle,
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

    let driven = driver::begin_top_level(app_id, schema, isolation_level, backend).await?;
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
pub async fn run_on_tx_conn(app_id: &str, sql: &str) -> Result<(), DbError> {
    driver::run_operation(app_id, sql, &[]).await
}

// ---------------------------------------------------------------------------
// run_begin_continuation — mint view, call callback, attach handlers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// V8 handler callbacks — reclaim the finalizer, spawn the settle op
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// exec_settle — COMMIT / ROLLBACK / RELEASE / ROLLBACK TO
// ---------------------------------------------------------------------------

/// Result of the settle SQL, carrying enough to build the outer promise's
/// `ResolveValue`.
#[derive(Debug)]
pub enum SettleOutcome {
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
pub async fn exec_settle(
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

pub fn commit_failed_indeterminate(error: DbError) -> DbError {
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

/// Whether this app still has an active transaction frame in the host context.
pub fn is_active(app_id: &str) -> bool {
    crate::tx_lanes::with(|lanes| lanes.has_tx_for(app_id))
}

#[cfg(test)]
thread_local! {
    /// The backend `install_sqlite_backend_for_test` opened, for [`test_backend`].
    ///
    /// It used to be parked in the ADAPTER's per-isolate context and read back
    /// out. The engine cannot name that context now, and never needed to: the
    /// fixture opens the backend, so the fixture can hold it.
    static TEST_BACKEND: std::cell::RefCell<Option<crate::backend::BackendHandle>> =
        const { std::cell::RefCell::new(None) };
}

/// The backend the tests below already installed, for the `begin` argument.
///
/// Sync, and it can be: only the COLD path needs to await, and every test here
/// opens a SQLite backend before it begins a transaction. Production resolves
/// through `tx_scope::ensure_backend`, which owns the cold arm.
/// The fixture schema every in-file transaction test opens against.
///
/// Spelled once so a test cannot accidentally open on a schema other than the
/// tenant it names - which is the shape this typing change exists to make
/// visible.
#[cfg(test)]
fn test_schema() -> zeroship_data_sql::SchemaName {
    zeroship_data_sql::SchemaName::new("app_sqlite").expect("fixture schema name")
}

#[cfg(test)]
fn test_backend() -> crate::backend::BackendHandle {
    TEST_BACKEND.with(|slot| {
        slot.borrow()
            .clone()
            .expect("test must install a backend before beginning a transaction")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // THE LAST V8-SHAPED NAME IN THIS FILE IS GONE. Two arms here drove the
    // ADAPTER's `build_settle_resolve_value` through
    // `zeroship_runtime::state::ResolveValue`, to prove the engine's outcome
    // mapping survives lowering. They moved to `zeroship-data-v8`'s
    // `v8_classes/transaction.rs` with the data-engine cut - the lowering is
    // that function's, and the engine crate declares neither `v8` nor
    // `zeroship-runtime`, so the arms could not follow the module they tested.
    use std::path::PathBuf;
    use std::rc::Rc;

    use crate::backend::sqlite::SqliteBackend;
    use zeroship_data_orm::fixtures::DatabaseFixture;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    struct ContextReset;

    impl Drop for ContextReset {
        fn drop(&mut self) {
            crate::tx_lanes::with_mut(|l| {
                let _ = l.take_tx_client_for("app_sqlite");
                l.retire_transaction("app_sqlite");
                l.release_tx_claim("app_sqlite");
                l.clear_pending_emits_for("app_sqlite");
            });
            // The backend slot this clears is the fixture's own thread-local,
            // not the adapter's per-isolate pool: this crate cannot name that
            // one, and clearing it here was only ever a way to unpark a handle
            // the fixture itself had parked.
            super::TEST_BACKEND.with(|slot| slot.borrow_mut().take());
        }
    }

    fn install_sqlite_backend_for_test() -> (Rc<SqliteBackend>, tempfile::TempDir, ContextReset) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let backend = Rc::new(
            crate::backend_selection::new_sqlite_backend(
                PathBuf::from(dir.path()),
                crate::encryption::ProjectKeySource::unavailable(),
            )
            .expect("open sqlite backend"),
        );
        let reset = ContextReset;
        crate::tx_lanes::with_mut(|l| {
            let _ = l.take_tx_client_for("app_sqlite");
            l.retire_transaction("app_sqlite");
            l.release_tx_claim("app_sqlite");
            l.clear_pending_emits_for("app_sqlite");
        });
        super::TEST_BACKEND.with(|slot| {
            *slot.borrow_mut() = Some(crate::backend::BackendHandle::new(Rc::clone(&backend)));
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
                .execute_fixture_on(&probe, "CREATE TABLE notes (id INTEGER PRIMARY KEY)", &[])
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
                .await
                .expect("begin");

            let first =
                exec_begin_or_savepoint(true, None, "app_sqlite", test_schema(), test_backend())
                    .await
                    .expect("first nested frame")
                    .expect("a nested begin opens a frame");
            // Roll it back, which on PostgreSQL leaves the savepoint defined -
            // the precondition that makes a reused name dangerous.
            match exec_settle("app_sqlite", false, Some(first)).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok for the first frame settle, got {other:?}"),
            }

            let second =
                exec_begin_or_savepoint(true, None, "app_sqlite", test_schema(), test_backend())
                    .await
                    .expect("second nested frame")
                    .expect("a nested begin opens a frame");
            assert_ne!(
                first, second,
                "a frame id is minted from a monotonic sequence and never reused"
            );

            let names = crate::tx_lanes::with(|l| {
                l.transaction_reducer("app_sqlite")
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
    fn max_savepoint_depth_is_eight() {
        // Pin the proposal's depth cap so a future change is a deliberate
        // edit, not an accidental drift.
        assert_eq!(MAX_SAVEPOINT_DEPTH, 8);
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

    /// The probe reads and seeds through `op_conn`; writes that belong to the
    /// transaction go through [`run_on_tx_conn`], which is the only path onto
    /// `tx_conn`.
    ///
    /// Before SC-2 these tests used `fixture_session()` for the probe
    /// AND drove the transaction's writes through it, which worked only
    /// because both were the same single connection. That coupling is the
    /// divergence SC-2 retires, so the probe now names the lane it wants.
    #[test]
    fn sqlite_top_level_begin_ignores_isolation_and_commits() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(
                false,
                Some(zeroship_data_orm::error::IsolationLevel::Serializable),
                "app_sqlite",
                test_schema(),
                test_backend(),
            )
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
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after commit");
            assert_eq!(rows[0][0].as_deref(), Some("1"));
            assert!(!crate::tx_lanes::with(|l| l.has_tx_for("app_sqlite")));
        });
    }

    /// **An open transaction survives the thread's backend being cleared.**
    ///
    /// `ThreadDbContext::clear_pool` nulls `backend` and leaves `tx_conns`
    /// untouched, so a `register` with a changed URL puts the thread in a state
    /// where a live pinned session exists and the ambient backend does not.
    /// Until 2026-09-02 both the operation path and the settle path read that
    /// ambient handle to decide which vendor they were talking to:
    /// `exec_on_session` refused with `not_configured`, and `terminal` returned
    /// `Indeterminate`, so a transaction on a perfectly healthy connection lost
    /// its writes and had its session withdrawn.
    ///
    /// The session is the authority on how to talk to itself.
    /// [`crate::driver::Session`]'s two variants ARE the two
    /// `DatabaseFixture::Client` associated types, so the variant already names the
    /// vendor and no second handle is consulted.
    ///
    /// Restoring either read reddens this: re-add the `not_configured` guard to
    /// `exec_on_session` and the second insert panics; restore `terminal`'s
    /// `match (&backend, &client)` with its `_ => mismatch` arm and the commit
    /// comes back `Indeterminate` with zero rows committed.
    #[test]
    fn an_open_transaction_outlives_the_threads_backend_being_cleared() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            // Taken from the backend we own, not from the context, so it stays
            // usable as an oracle after the context's handle is dropped.
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
                .await
                .expect("begin sqlite tx");
            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('before')")
                .await
                .expect("insert before the backend is cleared");

            // The window this test exists for: ambient backend gone, pinned
            // session still open and still perfectly usable. The slot cleared
            // is the fixture's, which is where this crate's tests park a
            // backend now - the adapter's per-isolate pool is out of reach and
            // was never what `run_on_tx_conn` reads.
            super::TEST_BACKEND.with(|slot| slot.borrow_mut().take());
            assert!(crate::tx_lanes::with(|l| l.has_tx_for("app_sqlite")));

            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('after')")
                .await
                .expect("insert after the backend is cleared");

            match exec_settle("app_sqlite", true, None).await {
                SettleOutcome::Ok => {}
                other => panic!("expected Ok settle outcome, got {other:?}"),
            }

            let rows = probe
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after commit");
            assert_eq!(rows[0][0].as_deref(), Some("2"));
            assert!(!crate::tx_lanes::with(|l| l.has_tx_for("app_sqlite")));
        });
    }

    #[test]
    fn sqlite_top_level_reject_path_actively_rolls_back() {
        run(async {
            let (backend, _dir, _reset) = install_sqlite_backend_for_test();
            let probe = backend.autocommit_client();
            backend
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
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
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after rollback");
            assert_eq!(rows[0][0].as_deref(), Some("0"));
            assert!(!crate::tx_lanes::with(|l| l.has_tx_for("app_sqlite")));
        });
    }

    /// **Forced cleanup reaches an unreachable SQLite session through SC-2's
    /// canceller, and rolls it back rather than giving up on it.**
    ///
    /// The PostgreSQL arm of this property is
    /// `a_forced_cleanup_cancels_the_running_statement_and_keeps_the_connection`
    /// in `crates/zeroship-data-v8/tests/native_transaction.rs`; this is its dev-tier peer, and it is
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
    /// [`crate::tx_lanes::TxClientSlotGuard`], which is what ordinary CRUD does
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
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
                .await
                .expect("begin sqlite tx");
            run_on_tx_conn("app_sqlite", "INSERT INTO notes (title) VALUES ('doomed')")
                .await
                .expect("insert inside sqlite tx");

            // Another future owns the session. Forced cleanup cannot take it out
            // of the slot, so the only route left is the canceller captured when
            // the session was installed.
            let held =
                crate::tx_lanes::TxClientSlotGuard::take("app_sqlite").expect("hold the session");

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
                !crate::tx_lanes::with(|l| l.tx_session_withdrawn("app_sqlite")),
                "a proved cleanup withdraws nothing"
            );

            drop(held);

            let rows = probe
                .query("SELECT COUNT(*) FROM notes", &[])
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
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            exec_begin_or_savepoint(false, None, "app_sqlite", test_schema(), test_backend())
                .await
                .expect("begin sqlite tx");

            // No transaction anywhere in this call's async scope.
            backend
                .execute_fixture_on(
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
                .query("SELECT title FROM notes ORDER BY id", &[])
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
                .execute_fixture_on(
                    &probe,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            let frame = AtomicWriteFrame::begin(crate::exec::ambient_route_for_tests(
                "app_sqlite",
                test_backend(),
            ))
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
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count notes after frame cancellation");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("0"),
                "dropping an unsettled frame must roll back its writes"
            );
            assert!(!crate::tx_lanes::with(|l| {
                l.has_tx_for("app_sqlite") || l.tx_claimed_by("app_sqlite")
            }));
        });
    }
}
