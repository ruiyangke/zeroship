//! The SC-1 driver: the only place a reducer [`Action`] becomes I/O.
//!
//! [`super::reducer`] is pure - it owns no session, no client, no timer and no
//! future. This module is its counterpart: it holds the session, executes every
//! action the reducer emits, and feeds the outcome straight back in as the next
//! event. Nothing here decides a transition; every branch below is either
//! "perform this action" or "report what the backend did".
//!
//! ## The constraint that decides this file: WHEN the health oracle is sampled
//!
//! SC-1 (`docs/proposals/2026-08-26-sc1-transaction-protocol.md`, the
//! health-oracle paragraph under the cleanup-goal table) pins it:
//!
//! > On PostgreSQL `transaction_status()` returns `None` while a request is in
//! > flight, and a *failed* statement's trailing `ReadyForQuery` is not consumed
//! > when its `await` returns. Inside a poisoned block - where every data
//! > statement fails with `25P02` - **no retry makes the oracle answer**. Since
//! > `None` is indeterminate and indeterminate withdraws the session, a driver
//! > that samples on entry to `Cancelling` would withdraw a perfectly healthy
//! > connection on *every* forced cleanup.
//!
//! [`crate::backend::postgres::cleanup`] therefore issues the cleanup
//! `ROLLBACK` **first** and samples **after** it. The `ROLLBACK` succeeds from a
//! poisoned block, and answering it resolves the status byte.
//! `a_forced_cleanup_on_a_poisoned_block_keeps_a_healthy_connection` in
//! `tests/native_transaction.rs` is the arm that fails if the two are ever
//! reordered.
//!
//! **That rule is now enforced where the evidence is.** Both cleanup arms live
//! in their own backend and only [`CleanupAck`] crosses back, so this file
//! states the constraint but no longer implements it for either vendor.
//!
//! ## Forced cleanup CANCELS; it withdraws only when it cannot prove a rollback
//!
//! The second constraint on this file is that a force must not answer a slow
//! statement by destroying the connection. It used to: [`cleanup`] can only roll
//! a transaction back if it can reach the session, the session is out of the
//! slot for the whole of any statement, and the execution deadline fires
//! **because a statement is slow** - so the mechanism that exists to bound one
//! responded, in its own common case, by killing the backend.
//!
//! [`cancel_and_reclaim`] is the answer, and it does not need the session:
//! PostgreSQL's `CancelRequest` travels on a second connection and names the
//! backend by process id, and SQLite's is a message to the session actor. The
//! canceller is captured at [`install`] - the one moment we still own the client
//! - and lives in [`crate::context::ThreadDbContext::tx_cancellers`].
//!
//! Withdrawal remains the fallback and is still reached, by every route that
//! leaves the cleanup unproved: a cancellation that cannot be delivered, one the
//! server discards because nothing was running, a statement that does not
//! release the session within [`CANCEL_RECLAIM_GRACE`], and a `ROLLBACK` whose
//! oracle does not read `Idle`. It is no longer the FIRST answer.
//!
//! ## What a withdrawal has to defeat here, specifically
//!
//! [`Action::WithdrawSession`] says "destroy the physical connection rather than
//! returning it". On PostgreSQL the transaction session is a
//! [`compio_postgres::OwnedPooledClient`], whose `Drop` calls
//! `pool.return_client(entry)` - so **dropping a withdrawn session hands it to
//! the next borrower**, which is the exact opposite of the action. Withdrawal is
//! therefore [`destroy_session`], which closes the client's request channel
//! first: `Pool::return_client` checks `PoolEntry::is_pool_eligible`, that checks
//! `!client.is_closed()`, and a closed client is evicted and its capacity slot
//! released instead of being published as idle.
//!
//! The same closure has to survive a *race*: a withdrawal can land while some
//! other future holds the session out of the slot behind a
//! [`crate::tx_lanes::TxClientSlotGuard`], whose `Drop` puts it back. So
//! withdrawal also sets a per-app tombstone
//! ([`crate::context::ThreadDbContext::withdraw_tx_session`]) and
//! `put_tx_client_for` destroys anything that returns under it. Without that,
//! "withdrawn" would hold only for the sessions that happened to be in the slot.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::tx_lanes::TxConnection;
use zeroship_data_core::error::{
    BeginIntent, DbError, IsolationLevel, OpenSessionError, SessionSetupDisposition,
};
use crate::exec::{clear_pending_emits, drain_pending_emits_on_commit};

use super::reducer::deadline::{DeadlineGeneration, DeadlineKind};
use super::reducer::frames::{FrameClose, FrameId};
use super::reducer::identity::{
    AuthorityDomain, AuthorityIdentity, ExpectedAuthority, LifecycleState, MaskCeiling,
    ObservedAuthority, SchemaEpoch,
};
use super::reducer::{
    Action, BackendGeneration, BeginOutcome, CleanupAck, CleanupGoal, CommandToken, EventAuthority,
    SettleIntent, TerminalOutcome, TerminalResult, TxBudgets, TxEvent, TxProtocolError, TxReducer,
    TxReply,
};

thread_local! {
    /// The generation stamped on the next transaction session this thread opens,
    /// for SC-1's guard order step 4.
    ///
    /// **Thread-level, NOT per-lane**, which is why it is a module thread-local
    /// here rather than a `TxLane` field: a counter that restarted per lane
    /// would let a stale completion authenticate against a later session by
    /// arithmetic coincidence. A completion naming a generation the current
    /// session does not carry is stale, and that check is only as good as the
    /// counter never going backwards.
    ///
    /// **It has no `reset_for_tests`, deliberately.** It lived on
    /// `ThreadDbContext` until 2026-09-02, where `reset_context_for_tests`
    /// rebuilt the whole struct and so silently returned it to 0 - contradicting
    /// the "never reset" its own doc claimed. Nothing depended on the reset
    /// (the one test built a private context), and not offering one makes the
    /// documented property true rather than nearly true.
    ///
    /// Its only consumer is [`Action::IssueBegin`] below. It sat in the adapter
    /// purely because that is where the context was; no adapter code ever read
    /// or wrote it.
    static BACKEND_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Mint the generation for the next session opened on this thread.
fn next_backend_generation() -> u64 {
    BACKEND_GENERATION.with(|g| {
        let next = g.get() + 1;
        g.set(next);
        next
    })
}

/// The authority axis a transaction is admitted under, today.
///
/// **Deliberately opaque, and named for the axis rather than baked into it.**
/// [`AuthorityIdentity`] compares only for equality and never parses its key;
/// the operator is weighing decoupling apps from databases, which would re-key
/// this onto the database or the grant. That move must be a change *here* and
/// nowhere else, so nothing downstream of this function may look inside the key.
///
/// The incarnation is `0` because no authority record exists to read one from
/// yet. That is stated rather than hidden: until a lifecycle record is published
/// and observed, the classifier has nothing to disagree with and
/// [`observation_for`] echoes the expectation. The wiring is real; the *input*
/// is not yet.
fn expected_authority(app_id: &str) -> ExpectedAuthority {
    ExpectedAuthority {
        identity: AuthorityIdentity::for_app(app_id, 0),
        domain: AuthorityDomain::new(0, 0),
        epoch: SchemaEpoch::new(0),
    }
}

/// The authority observation the driver submits for `Preparing`.
///
/// See [`expected_authority`]: there is no lifecycle record to read, so this
/// echoes the expectation and the classifier returns `Current`. It runs anyway,
/// because the reducer re-runs the classifier on the observation itself - a
/// publisher cannot smuggle a verdict past it - and because the day a record
/// exists, this is the one function that has to change.
fn observation_for(expected: &ExpectedAuthority) -> ObservedAuthority {
    ObservedAuthority {
        identity: expected.identity.clone(),
        domain: expected.domain.clone(),
        epoch: expected.epoch,
        lifecycle: LifecycleState::Stable,
        // No ceiling is published yet, so the fold starts from the empty
        // ceiling. `meet` can only tighten it, which is invariant 8.
        ceiling: MaskCeiling::default(),
    }
}

/// What one driven step produced for its caller.
///
/// The reply is the **last** one the loop saw, which is the reply of the event
/// the loop finished on rather than the one it started from. That is the
/// intended reading: a `BEGIN` that failed does not answer `Began`, it answers
/// the `Settled(Cancelled(BeginFailed))` its forced cleanup reached.
#[derive(Debug, Default)]
pub(crate) struct Driven {
    pub(crate) reply: Option<Result<TxReply, TxProtocolError>>,
    /// The backend error behind a failed step, kept so the creator sees the
    /// server's message rather than only the protocol's classification.
    pub(crate) error: Option<DbError>,
    /// The token an [`Action::IssueDataSql`] minted, carried out of the step
    /// that emitted it so the caller can report the matching completion. The
    /// reducer hands the session over; the SQL is the caller's.
    issued_operation: Option<CommandToken>,
}

impl Driven {
    /// The outcome recorded on the reply, if this step settled the transaction.
    pub(crate) fn outcome(&self) -> Option<TerminalOutcome> {
        match &self.reply {
            Some(Ok(TxReply::Settled(outcome))) => Some(*outcome),
            _ => None,
        }
    }

    /// The frame this step opened or closed, if any.
    pub(crate) fn frame(&self) -> Option<FrameId> {
        match &self.reply {
            Some(Ok(TxReply::FrameOpened(id) | TxReply::FrameClosed(id))) => Some(*id),
            _ => None,
        }
    }

    /// The protocol refusal, if the step was refused.
    pub(crate) fn refusal(&self) -> Option<TxProtocolError> {
        match &self.reply {
            Some(Err(error)) => Some(*error),
            _ => None,
        }
    }
}

/// Per-step inputs the reducer does not model but the driver needs.
#[derive(Debug, Default, Clone)]
pub(crate) struct StepConfig {
    /// How to open the transaction, for the step that runs
    /// [`Action::IssueBegin`].
    ///
    /// **An intent, never a statement.** This carried a rendered
    /// `BEGIN [ISOLATION LEVEL ...]` string until 2026-09-02 - PostgreSQL
    /// dialect threaded through the vendor-neutral state machine, which the
    /// SQLite arm then ignored in favour of a hardcoded `BEGIN`. Each lane
    /// spells the intent now.
    pub(crate) begin: BeginIntent,

    /// The backend [`Action::IssueBegin`] opens its session on.
    ///
    /// **Optional because the type is shared with seven step paths that
    /// provably never open one**, not because opening is optional. `IssueBegin`
    /// is constructed in exactly one place in the reducer, and the only route
    /// to it is [`begin_top_level`], which always supplies `Some`. The
    /// `StepConfig::default()` callers - open_frame, close_frame, settle_root,
    /// run_operation, deadline_fired, cancel - drive events that cannot emit it.
    ///
    /// A route cannot ride here instead: `TxRoute` is deliberately neither
    /// `Clone` nor `Default` (`crate::tx_route`), which this struct is both,
    /// and `deadline_fired` runs on a timer with no V8 scope to capture from.
    pub(crate) backend: Option<crate::backend::BackendHandle>,
}

/// Admit a top-level transaction, then drive it to `Idle`.
///
/// The admission claim is taken by the caller before this runs - it has to be,
/// because SC-1 arms the execution deadline on the same transition that grants
/// admission, and admission is what serialises two top-level transactions for
/// one app.
///
/// **DBR-11 closes here.** The reducer's `settle_now` emits
/// [`Action::ReleaseAdmission`] on *every* path to `Settled`, including the ones
/// that never sent a `BEGIN`, so a cancelled admission cannot leave the claim
/// held. The orchestrator no longer has to remember to release it per-arm.
pub(crate) async fn begin_top_level(
    app_id: &str,
    isolation_level: Option<IsolationLevel>,
    backend: crate::backend::BackendHandle,
) -> Result<Driven, DbError> {
    let begin = isolation_level.map_or(BeginIntent::Default, BeginIntent::Isolation);
    let admit = admit_in_preparing(app_id);
    let config = StepConfig {
        begin,
        backend: Some(backend),
    };
    // The admission actions are only ever `ScheduleTimer`; run them through the
    // same interpreter so no action has a second, quieter implementation.
    let mut driven = run(app_id, admit, &config).await;
    let Some(authority) = authority_of(app_id) else {
        return Err(DbError::internal(
            "db.transaction: admission did not install a reducer",
        ));
    };
    let observed = observation_for(&crate::tx_lanes::with(|l| {
        l.transaction_expected_authority(app_id)
            .cloned()
            .expect("just admitted")
    }));
    let began = step(
        app_id,
        TxEvent::AuthorityObserved {
            authority,
            observed: Box::new(observed),
        },
        &config,
    )
    .await;
    driven.absorb(began);
    Ok(driven)
}

/// Admit a transaction and return admission's actions, leaving it in
/// `Preparing`.
///
/// Split out of [`begin_top_level`] because `Preparing` is the only state that
/// fixes the `NoTransaction` cleanup goal, and an arm that must reach it cannot
/// go through a function that leaves `Preparing` in the same call.
pub(crate) fn admit_in_preparing(app_id: &str) -> Vec<Action> {
    crate::tx_lanes::with_mut(|l| {
        l.admit_transaction(
            app_id,
            expected_authority(app_id),
            budgets(),
            Instant::now(),
            super::MAX_SAVEPOINT_DEPTH,
        )
    })
}

/// Open a nested frame: `SAVEPOINT <the reducer's monotonic name>`.
///
/// The name is **never** derived from the nesting depth. `ROLLBACK TO SAVEPOINT`
/// leaves the savepoint defined and PostgreSQL resolves a name to the most
/// recently established one, so a depth-derived name is reused after the depth
/// decrements and the enclosing frame's rollback lands in the wrong scope. See
/// [`super::reducer::frames`].
pub(crate) async fn open_frame(app_id: &str) -> Driven {
    step(app_id, TxEvent::OpenFrame, &StepConfig::default()).await
}

/// Close a nested frame with `RELEASE` or `ROLLBACK TO` + `RELEASE`.
pub(crate) async fn close_frame(app_id: &str, frame: FrameId, close: FrameClose) -> Driven {
    step(
        app_id,
        TxEvent::CloseFrame { frame, close },
        &StepConfig::default(),
    )
    .await
}

/// Settle the root with `COMMIT` or `ROLLBACK`.
///
/// **DBR-03 closes here.** There is no "the slot is empty, so treat it as
/// settled" arm any more: the reducer decides, and the only state that ends a
/// settle without sending anything is `Settled` itself. A settle arriving while
/// an operation owns the session parks in `Quiescing` and issues its terminal
/// SQL when the operation returns.
pub(crate) async fn settle_root(app_id: &str, intent: SettleIntent) -> Driven {
    step(
        app_id,
        TxEvent::SettleRequested { intent },
        &StepConfig::default(),
    )
    .await
}

/// Run one creator data statement under the reducer's operation guard.
///
/// This is what makes `Poisoned` a state the driver can actually reach: a
/// statement that errors reports `errored: true`, and the reducer parks the
/// transaction where PostgreSQL has already put it.
///
/// **No production caller yet, and that is a stated gap rather than an
/// oversight.** Creator CRUD issued inside a transaction goes through
/// `exec::run_sql` / `exec::exec_sqlite_json`, which take the session with a
/// bare [`crate::tx_lanes::TxClientSlotGuard`] and report nothing to the state
/// machine. Routing them here is the same work the module header already names
/// as open - "capturing the async scope at each CRUD dispatch site" - and it is
/// not folded into this change because every one of those call sites has tests
/// that install a transaction session with no reducer behind it. Until then a
/// failed creator statement leaves the reducer reading `Idle` while PostgreSQL
/// reads `Failed`; forced cleanup still handles it correctly, because the goal
/// `OpenTransaction` fixes from `Idle` and `Poisoned` alike and the health
/// oracle is sampled from the server rather than from the reducer.
#[allow(
    dead_code,
    reason = "exercised by transaction::run_on_tx_conn's tests; see the paragraph above \
              for the production call sites that must move onto it"
)]
pub(crate) async fn run_operation(app_id: &str, sql: &str, params: &[&str]) -> Result<(), DbError> {
    let started = step(app_id, TxEvent::OperationRequested, &StepConfig::default()).await;
    if let Some(refusal) = started.refusal() {
        return Err(protocol_error(refusal, started.error));
    }
    let Some(token) = started.issued_operation else {
        return Err(DbError::internal(
            "db: the reducer accepted an operation without issuing one",
        ));
    };

    let result = exec_on_session(app_id, sql, params).await;
    let errored = result.is_err();
    let finished = step(
        app_id,
        TxEvent::OperationCompleted { token, errored },
        &StepConfig::default(),
    )
    .await;
    // The statement's own error is what the creator must see; the reducer's
    // `TransactionNotReady` reply on the errored arm is the state transition,
    // not the diagnosis.
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = finished;
            Err(error)
        }
    }
}

/// Deliver an expired timer.
///
/// The task that calls this carries only the app key, the kind and the
/// generation - no session, no client, no settle future - which is what makes
/// SC-1 rule 4's "independent of callback behaviour" true rather than
/// aspirational.
pub(crate) async fn deadline_fired(
    app_id: &str,
    kind: DeadlineKind,
    generation: DeadlineGeneration,
) -> Driven {
    step(
        app_id,
        TxEvent::DeadlineFired { kind, generation },
        &StepConfig::default(),
    )
    .await
}

/// Force this transaction to end under [`CleanupCause::Cancelled`].
///
/// The event carries the authority the transaction was admitted under, so guard
/// order step 1 has something to compare: a cancel naming a different identity
/// must not touch this entry's session, actor, timer or admission - not even to
/// cancel it.
#[allow(
    dead_code,
    reason = "no production cancel publisher yet - the creator API has no tx.cancel(), \
              and the deadline path reaches Cancelling through `deadline_fired`. \
              Exercised by `transaction::probe::cancel`."
)]
pub(crate) async fn cancel(app_id: &str) -> Driven {
    let Some(authority) = authority_of(app_id) else {
        return Driven::default();
    };
    step(
        app_id,
        TxEvent::Cancel { authority },
        &StepConfig::default(),
    )
    .await
}

// ---------------------------------------------------------------------------
// The interpreter
// ---------------------------------------------------------------------------

impl Driven {
    /// Fold a later step's answers over this one's. Later wins, per field, so a
    /// step that produced no reply does not erase the one before it.
    fn absorb(&mut self, other: Self) {
        if other.reply.is_some() {
            self.reply = other.reply;
        }
        if other.error.is_some() {
            self.error = other.error;
        }
        if other.issued_operation.is_some() {
            self.issued_operation = other.issued_operation;
        }
    }
}

/// Apply one event and run every action it produces, to quiescence.
async fn step(app_id: &str, event: TxEvent, config: &StepConfig) -> Driven {
    let Some(actions) = apply(app_id, event) else {
        // No reducer for this app: no transaction is admitted. This is the one
        // "there is nothing here" answer, and it is a REFUSAL rather than a
        // silent success - reading absence as "already settled" is the shape
        // DBR-03 turned into a false commit.
        return Driven {
            reply: Some(Err(TxProtocolError::TransactionNotReady)),
            ..Driven::default()
        };
    };
    run(app_id, actions, config).await
}

fn apply(app_id: &str, event: TxEvent) -> Option<Vec<Action>> {
    let now = Instant::now();
    crate::tx_lanes::with_mut(|l| l.apply_transaction_event(app_id, event, now))
}

fn authority_of(app_id: &str) -> Option<EventAuthority> {
    crate::tx_lanes::with(|l| {
        l.transaction_expected_authority(app_id)
            .map(|expected| EventAuthority {
                identity: expected.identity.clone(),
                domain: expected.domain.clone(),
            })
    })
}

/// Interpret actions in order, feeding each completion straight back in.
///
/// Ordering is load-bearing and is the reducer's, not this loop's: `settle_now`
/// emits effects -> session disposition -> admission release -> reply, and a
/// loop that reordered them would publish a commit's events after the session
/// was already back in the pool.
async fn run(app_id: &str, actions: Vec<Action>, config: &StepConfig) -> Driven {
    let mut driven = Driven::default();
    let mut queue: VecDeque<Action> = actions.into();

    while let Some(action) = queue.pop_front() {
        match action {
            Action::Reply(reply) => driven.reply = Some(reply),

            Action::ScheduleTimer(scheduled) => schedule_timer(app_id, scheduled),

            Action::IssueBegin { token } => {
                let generation = next_backend_generation();
                // `begin_top_level` is the only route to this action and it
                // always supplies the backend; see `StepConfig::backend`. The
                // absent arm is an internal error rather than a fallback,
                // because a fallback here would resolve one of its own and put
                // the read back in the engine.
                let Some(backend) = config.backend.as_ref() else {
                    driven.error = Some(DbError::internal(
                        "db.transaction: begin was issued without a backend",
                    ));
                    queue.push_back(Action::Reply(Err(TxProtocolError::TransactionNotReady)));
                    continue;
                };
                let outcome = match open_session(app_id, config.begin, backend).await {
                    Ok(()) => BeginOutcome::Opened(BackendGeneration(generation)),
                    Err(OpenSessionError::Failed(error)) => {
                        driven.error = Some(error);
                        BeginOutcome::Failed
                    }
                    Err(OpenSessionError::Setup(setup)) => {
                        let disposition = setup.disposition();
                        driven.error = Some(setup.into_db_error());
                        match disposition {
                            SessionSetupDisposition::Preserve => BeginOutcome::SetupFailed,
                            SessionSetupDisposition::ReResolve => BeginOutcome::ReResolve,
                            SessionSetupDisposition::Denied(reason) => BeginOutcome::Denied(reason),
                            SessionSetupDisposition::Failed => BeginOutcome::Failed,
                        }
                    }
                };
                extend(
                    &mut queue,
                    app_id,
                    TxEvent::BeginCompleted { token, outcome },
                );
            }

            Action::IssueDataSql { token } => {
                // The statement itself is run by `run_operation`, which owns the
                // SQL; the reducer only says "the session is yours now".
                driven.issued_operation = Some(token);
            }

            Action::IssueSavepoint { token, name } => {
                let frame = current_frame(app_id);
                let ok = record(
                    &mut driven,
                    exec_on_session(app_id, &format!("SAVEPOINT {name}"), &[]).await,
                );
                if ok {
                    crate::tx_lanes::with_mut(|l| l.push_frame_emit_mark(app_id));
                }
                let Some(frame) = frame else {
                    driven.error = Some(missing_frame());
                    continue;
                };
                extend(
                    &mut queue,
                    app_id,
                    TxEvent::OpenFrameCompleted { token, frame, ok },
                );
            }

            Action::IssueRollbackTo { token, name } => {
                let frame = current_frame(app_id);
                let ok = record(
                    &mut driven,
                    exec_on_session(app_id, &format!("ROLLBACK TO SAVEPOINT {name}"), &[]).await,
                );
                if ok {
                    // The frame's queued events are discarded only now that the
                    // statement has succeeded and the rows they describe are
                    // known to be gone. Discarding first makes the failure row's
                    // documented fate - retain the evidence, poison the
                    // transaction - unachievable.
                    crate::tx_lanes::with_mut(|l| l.discard_frame_effects(app_id));
                }
                let Some(frame) = frame else {
                    driven.error = Some(missing_frame());
                    continue;
                };
                extend(
                    &mut queue,
                    app_id,
                    TxEvent::CloseFrameCompleted {
                        token,
                        frame,
                        close: FrameClose::RolledBackTo,
                        ok,
                    },
                );
            }

            Action::IssueRelease { token, name } => {
                let frame = current_frame(app_id);
                let ok = record(
                    &mut driven,
                    exec_on_session(app_id, &format!("RELEASE SAVEPOINT {name}"), &[]).await,
                );
                if ok {
                    // A released frame's events belong to the enclosing frame
                    // now, exactly as its rows do: pop the watermark without
                    // truncating.
                    crate::tx_lanes::with_mut(|l| l.pop_frame_emit_mark(app_id));
                }
                let Some(frame) = frame else {
                    driven.error = Some(missing_frame());
                    continue;
                };
                extend(
                    &mut queue,
                    app_id,
                    TxEvent::CloseFrameCompleted {
                        token,
                        frame,
                        close: FrameClose::Released,
                        ok,
                    },
                );
            }

            Action::IssueTerminal { token, intent } => {
                let (result, error) = terminal(app_id, intent).await;
                if let Some(error) = error {
                    driven.error = Some(error);
                }
                extend(
                    &mut queue,
                    app_id,
                    TxEvent::TerminalCompleted { token, result },
                );
            }

            Action::IssueCancellation { token, goal } => {
                let ack = cleanup(app_id, token, goal).await;
                extend(
                    &mut queue,
                    app_id,
                    TxEvent::CancellationAcknowledged { token, ack },
                );
            }

            Action::PublishEffects(_) => drain_pending_emits_on_commit(app_id),
            Action::DiscardEffects => clear_pending_emits(app_id),

            Action::WithdrawSession => destroy_session(app_id),
            Action::ReleaseSession => release_session(app_id),

            Action::ReleaseAdmission => {
                crate::tx_lanes::with_mut(|l| {
                    l.retire_transaction(app_id);
                    l.release_tx_claim(app_id);
                });
            }
        }
    }
    driven
}

/// Apply a follow-up event and append its actions to the queue.
///
/// Appended, not prepended: the reducer emitted the actions ahead of this one in
/// the order it wants them run, and a completion's consequences come after them.
fn extend(queue: &mut VecDeque<Action>, app_id: &str, event: TxEvent) {
    if let Some(actions) = apply(app_id, event) {
        queue.extend(actions);
    }
}

/// Latch a statement's error and report whether it succeeded.
fn record(driven: &mut Driven, result: Result<(), DbError>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            driven.error = Some(error);
            false
        }
    }
}

/// The frame the reducer just pushed, or is about to close.
///
/// `Action::IssueSavepoint` / `IssueRelease` / `IssueRollbackTo` carry the
/// savepoint NAME but not the frame id, so the completion's id is read back off
/// the stack. The top frame is the only one that can be acting - strict LIFO is
/// the frame stack's invariant, not an assumption made here.
/// The error for a frame action whose frame is not on the stack.
///
/// Unreachable through any sequence of public calls - the reducer pushes or
/// selects the frame in the same `apply` that emitted the action - but it is a
/// value the caller receives rather than a silent `continue`, because a frame
/// completion that is never fed leaves the transaction in `InFlight` until its
/// deadline fires.
fn missing_frame() -> DbError {
    DbError::internal("db.transaction: a frame action named no frame on the stack")
}

fn current_frame(app_id: &str) -> Option<FrameId> {
    crate::tx_lanes::with(|l| {
        l.transaction_reducer(app_id).and_then(|reducer| {
            reducer
                .frames()
                .top()
                .map(super::reducer::frames::Frame::id)
        })
    })
}

// ---------------------------------------------------------------------------
// The I/O each action turns into
// ---------------------------------------------------------------------------

/// Ask the backend for an open, narrowed session and install it.
///
/// **The protocol's half of opening, which is the whole of what SC-1 knows
/// about it.** How a session is acquired, what dialect spells the `BEGIN`, and
/// what narrowing its authority means are
/// [`BackendHandle::open_tx_session`]'s; this decides only that a session is
/// due and where it goes.
///
/// Every later action runs on that same narrowed session rather than on a fresh
/// checkout - data SQL, `SAVEPOINT`, `RELEASE`, `ROLLBACK TO`, terminal SQL and
/// the forced-cleanup `ROLLBACK` - so this driver has no ambient-privilege path
/// to lose if the worker role stops inheriting app roles.
async fn open_session(
    app_id: &str,
    begin: BeginIntent,
    backend: &crate::backend::BackendHandle,
) -> Result<(), OpenSessionError> {
    install(app_id, backend.open_tx_session(app_id, begin).await?);
    // Drop any broker residue from an interrupted prior run so it cannot leak
    // into this transaction's drain.
    clear_pending_emits(app_id);
    Ok(())
}

/// Park the session in the app's slot, and record how to cancel it.
///
/// **The canceller is captured HERE, not where it is used.** Forced cleanup
/// needs it precisely when the client is unreachable - some other future is
/// holding it out of the slot - so the one moment it can be taken is the one
/// moment we still own the client.
fn install(app_id: &str, client: TxConnection) {
    let canceller = client.canceller();
    crate::tx_lanes::with_mut(|l| {
        let previous = l.install_tx_client(app_id, client);
        debug_assert!(
            previous.is_none(),
            "open_session: the tx slot was already occupied for this app"
        );
        if let Some(canceller) = canceller {
            l.install_tx_canceller(app_id, canceller);
        }
    });
}

/// Run one statement on the app's pinned transaction session.
async fn exec_on_session(app_id: &str, sql: &str, params: &[&str]) -> Result<(), DbError> {
    let client = crate::tx_lanes::TxClientSlotGuard::take(app_id)?;
    client.client().exec(sql, params).await.map(|_| ())
}

/// Send terminal SQL and classify what the backend actually did.
///
/// **The command tag is not cosmetic.** PostgreSQL answers `COMMIT` with the tag
/// `ROLLBACK` when the transaction is in a failed state, and a driver that reads
/// only "did it error" reports a discarded transaction as committed - which is
/// what published change events for writes that never landed. The check stays
/// scoped to the PostgreSQL `COMMIT` arm: `RELEASE` answers with the tag
/// `RELEASE`, so "anything but COMMIT is a failure" would reject every healthy
/// nested commit.
///
/// **The driver calls a method on the session; it never matches its variants.**
/// [`crate::tx_lanes::TxConnection::settle`] owns the dialect and the projection,
/// so this function is the protocol's half alone: take the session, ask it to
/// settle, put it back for the disposition action the reducer emits next.
async fn terminal(app_id: &str, intent: SettleIntent) -> (TerminalResult, Option<DbError>) {
    let Some(client) = crate::tx_lanes::with_mut(|l| l.take_tx_client_for(app_id)) else {
        // The session is gone before terminal SQL was sent. This does NOT prove
        // the transaction ended - that inference is DBR-03 - so it is
        // indeterminate and the reducer withdraws.
        return (
            TerminalResult::Indeterminate,
            Some(DbError::internal(
                "db: the transaction session was unavailable when terminal SQL was due",
            )),
        );
    };

    let outcome = client.settle(intent).await;

    // The session is disposed of by `Action::ReleaseSession` / `WithdrawSession`,
    // which the reducer emits next, so put it back for that action to act on.
    // Returning it here rather than dropping it is what lets the withdrawal
    // arm reach the physical connection at all.
    crate::tx_lanes::with_mut(|l| l.put_tx_client_for(app_id, client));
    outcome
}

/// How long forced cleanup waits for a cancelled statement's holder to hand the
/// session back, once the cancellation is known to have been delivered.
///
/// **Not "how long may cleanup take" - the reducer's `cancellation_sql` budget
/// is that, and it is enforced by a timer in its own task.** This bounds a
/// narrower thing: how long after the postmaster confirmed it consumed the
/// `CancelRequest` the backend may still be running the statement. PostgreSQL
/// raises `57014` at the next `CHECK_FOR_INTERRUPTS`, so an interruptible
/// statement returns in about a round trip; one that does not is a session we do
/// not want back, and withdrawal is the cheap, safe answer. Sitting out the full
/// cancellation budget instead would hold the admission claim - and, on a pool
/// sized like ours, a connection slot - for a session that is going to be
/// destroyed anyway.
///
/// It is deliberately shorter than `TxBudgets::cancellation_sql` so that the
/// answer a transaction settles on comes from this function rather than from the
/// second-stage deadline racing it.
pub(crate) const CANCEL_RECLAIM_GRACE: Duration = Duration::from_secs(1);

/// Which cleanup, of which session, a resumed future belongs to.
///
/// **Cancellation made this necessary.** Cleanup used to be a straight line with
/// no await between reading the slot and writing it back, so "the session in the
/// slot" could only ever be the one being cleaned up. Now cleanup waits, and the
/// wait can outlive the transaction: the `CancellationSql` deadline fires in its
/// own task, settles `Indeterminate`, withdraws, and releases the admission - at
/// which point the next transaction is admitted, clears the withdrawal tombstone
/// and installs ITS session in this very slot. A resumed cleanup that took
/// whatever it found there would issue `ROLLBACK` on a healthy, unrelated
/// transaction.
///
/// Both halves are load-bearing. The token alone is not enough: tokens restart
/// at 1 in every reducer, so a later transaction forced from `Preparing` mints
/// the same value. The backend generation is monotonic for the life of the
/// thread and never reset, so it cannot recur - it is guard order step 4's
/// counter, reused here for the identity it already provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CleanupIdentity {
    token: CommandToken,
    generation: BackendGeneration,
}

impl CleanupIdentity {
    /// Read the identity of the cleanup `token` belongs to, or `None` if it is
    /// not the cleanup this app's reducer is currently running.
    fn capture(app_id: &str, token: CommandToken) -> Option<Self> {
        crate::tx_lanes::with(|l| Self::read(l, app_id, token))
    }

    /// Is this still the cleanup the app's reducer is running?
    /// Takes the LANES, not the whole context: everything it reads is lane
    /// state (`transaction_reducer`). It took `&ThreadDbContext` until
    /// 2026-09-02, which forced its caller to hold an adapter borrow to answer
    /// an engine question.
    fn is_current(self, lanes: &crate::tx_lanes::TxLanes, app_id: &str) -> bool {
        Self::read(lanes, app_id, self.token) == Some(self)
    }

    fn read(
        lanes: &crate::tx_lanes::TxLanes,
        app_id: &str,
        token: CommandToken,
    ) -> Option<Self> {
        let reducer = lanes.transaction_reducer(app_id)?;
        if reducer.state() != super::reducer::TxState::Cancelling
            || reducer.cancellation_token() != Some(token)
        {
            return None;
        }
        Some(Self {
            token,
            generation: reducer.generation()?,
        })
    }
}

/// Wait for a cancelled command to hand the transaction session back.
///
/// Resolves as soon as the slot refills OR the cleanup it belongs to stops
/// being the current one - the second is what stops a waiter sitting out its
/// whole grace after its transaction was retired underneath it.
struct SessionReturned<'a> {
    app_id: &'a str,
    identity: CleanupIdentity,
}

impl std::future::Future for SessionReturned<'_> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let ready = crate::tx_lanes::with_mut(|l| {
            if l.has_tx_for(self.app_id) || !self.identity.is_current(l, self.app_id) {
                return true;
            }
            l.push_tx_slot_waiter(self.app_id, cx.waker());
            false
        });
        if ready {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}

/// Perform forced cleanup and report what the health oracle said **afterwards**.
///
/// This function is the one SC-1 constrains by name. Read the module docs before
/// touching the order of the two steps inside [`cleanup_postgres`].
async fn cleanup(app_id: &str, token: CommandToken, goal: CleanupGoal) -> CleanupAck {
    if let Some(ack) = rollback_session_in_slot(app_id).await {
        return ack;
    }

    // An empty slot has three causes and they are NOT the same, so the answer is
    // taken from the PROTOCOL's view of session ownership rather than from the
    // slot being empty. Reading emptiness as proof of anything is the DBR-03
    // shape.
    let session = crate::tx_lanes::with(|l| l.transaction_reducer(app_id).map(TxReducer::session));
    match session {
        // No session was ever acquired. From `Preparing` - goal `NoTransaction` -
        // that is proved by construction with no I/O: the reducer mints the
        // `IssueBegin` token on the transition OUT of `Preparing`, so a force
        // that found `Preparing` interrupted a transaction whose `BEGIN` was
        // never issued.
        //
        // **From `Starting` - goal `AbortIfOpened` - it is NOT proved, and this
        // arm answers it anyway.** `open_session` acquires the client, runs
        // `BEGIN`, applies the per-app role and only THEN installs, so a force
        // landing inside that window sees `SessionOwnership::None` while a
        // transaction may already be open on the server. This is pre-existing
        // and untouched here, and it is stated rather than papered over: an
        // earlier version of this comment claimed "nothing can be open", which is
        // false for exactly one of the two states that reach it.
        //
        // Its consequence is bounded but real. The transaction settles
        // `Cancelled`, `open_session` then installs a live in-transaction client
        // into a retired transaction's slot, and it is evicted by the NEXT
        // transaction's `install_tx_client` - whose returned previous occupant
        // drops into `Pool::return_client`, which rolls back any session it
        // cannot prove `Idle`. Closing it properly needs `install` to consult the
        // withdrawal tombstone and to carry a generation, which is a change to
        // the BEGIN path rather than to cleanup.
        //
        // Cancellation cannot help here: the canceller is captured at `install`,
        // and this is precisely the window before it exists.
        Some(super::reducer::SessionOwnership::None) => {
            let _ = goal;
            CleanupAck::NoOpenTransaction
        }
        // A session exists and some other future holds it out of the slot. This
        // is the COMMON case, not an edge: the execution deadline fires because
        // a statement is slow, and a slow statement is one whose future is
        // holding the session. Cancel it rather than concluding that health is
        // unknown - the previous answer here destroyed the connection every
        // time the mechanism that exists to bound a slow statement fired.
        //
        // `Registry` and `Command` are both real here, and the difference is not
        // one this function may act on. `Command` is a statement the reducer
        // knows about; `Registry` is the ambient CRUD path, which takes the
        // session with a bare `TxClientSlotGuard` and reports nothing to the
        // state machine (see the module header's "known gap"). Today's
        // production creator statements are the second kind.
        Some(
            super::reducer::SessionOwnership::Registry
            | super::reducer::SessionOwnership::Command(_),
        ) => cancel_and_reclaim(app_id, token).await,
        // Already withdrawn, or no reducer at all. Nothing to prove and nothing
        // to cancel.
        Some(super::reducer::SessionOwnership::Withdrawn) | None => CleanupAck::Indeterminate,
    }
}

/// Roll back whatever session is parked in the app's slot.
///
/// `None` means the slot was empty - which is a question about ownership, not an
/// answer, and the caller resolves it.
async fn rollback_session_in_slot(app_id: &str) -> Option<CleanupAck> {
    let client = crate::tx_lanes::with_mut(|l| l.take_tx_client_for(app_id))?;

    let ack = client.cleanup().await;

    // Put it back so the reducer's session disposition can act on it.
    crate::tx_lanes::with_mut(|l| l.put_tx_client_for(app_id, client));
    Some(ack)
}

/// Cancel the statement holding the session, reclaim it, and roll it back.
///
/// The three steps are in this order for a reason that is not stylistic:
///
/// 1. **Deliver the cancellation and wait for the server to confirm it.**
///    `Pool::cancel_query` returns only after the postmaster closed the
///    dedicated cancellation connection, which is the cross-connection ordering
///    barrier. Until that returns, a `CancelRequest` is still in flight and
///    could land on any statement this backend runs next - including the
///    `ROLLBACK` below, or a later borrower's query.
/// 2. **Wait for the holder to give the session back.** The cancelled statement
///    fails with `57014`, its `await` returns, and its
///    [`crate::tx_lanes::TxClientSlotGuard`] parks the session. That put is what
///    wakes this wait.
/// 3. **Roll back and sample the oracle**, in that order, exactly as
///    [`cleanup_postgres`] does for a session that was in the slot all along.
///
/// ## What happens when the cancel and the return race
///
/// The canceller and the holder run concurrently, so all three orderings must be
/// safe, and they are - two of them by construction and one by falling back:
///
/// * The cancel lands while the statement runs. The intended case: `57014`, the
///   guard returns the session, the `ROLLBACK` proves the goal, the connection
///   is kept.
/// * The statement finishes on its own first. Step 1 has not returned yet, so
///   nothing here touches the session while the packet is unconfirmed. The
///   server discards a cancel that arrives while a backend is idle waiting for a
///   command, so the `ROLLBACK` runs normally.
/// * The cancel is dispatched in the narrow window after the statement finished
///   but before the backend is back at command-read. The pending cancel is
///   consumed by the next interrupt check, which is our own `ROLLBACK`: it fails
///   with `57014`, the oracle does not read `Idle`, the ack is `Indeterminate`
///   and the session is withdrawn. A false withdrawal - the safe answer - and
///   never a cancel that outlives this cleanup, because step 1's EOF proves the
///   packet was consumed before the `ROLLBACK` was even sent.
async fn cancel_and_reclaim(app_id: &str, token: CommandToken) -> CleanupAck {
    let Some(identity) = CleanupIdentity::capture(app_id, token) else {
        return CleanupAck::Indeterminate;
    };
    let Some(canceller) = crate::tx_lanes::with(|l| l.tx_canceller_for(app_id)) else {
        // No canceller was captured for this session. A SQLite handle with no
        // transaction reservation is the only way to get here, and there is
        // nothing stable to interrupt.
        return CleanupAck::Indeterminate;
    };

    match canceller.cancel().await {
        Ok(crate::backend::cancel::CancelDelivery::SqliteSettled(outcome)) => {
            // SQLite's actor rolls back and retires the reservation BEFORE it
            // acknowledges, so its answer is the cleanup result. There is
            // nothing left to reclaim.
            return match crate::backend::sqlite::reservation::terminal_result(&outcome).0 {
                TerminalResult::RolledBack => CleanupAck::RolledBack,
                TerminalResult::Committed | TerminalResult::Indeterminate => {
                    CleanupAck::Indeterminate
                }
            };
        }
        Ok(crate::backend::cancel::CancelDelivery::Requested) => {}
        Err(error) => {
            tracing::warn!(
                app_id,
                error = %error.message_str(),
                "sc1: forced cleanup could not deliver a cancellation request; the \
                 session's health is unknown and it will be withdrawn"
            );
            return CleanupAck::Indeterminate;
        }
    }

    let reclaimed =
        compio::time::timeout(CANCEL_RECLAIM_GRACE, SessionReturned { app_id, identity }).await;
    if reclaimed.is_err() {
        tracing::warn!(
            app_id,
            grace = ?CANCEL_RECLAIM_GRACE,
            "sc1: a cancelled statement did not release the transaction session \
             within the reclaim grace; the session will be withdrawn"
        );
        return CleanupAck::Indeterminate;
    }

    // The wait resolves on EITHER the slot refilling or this cleanup ceasing to
    // be the current one, so the identity has to be re-read rather than assumed.
    // Skipping this check is what would let a cleanup whose transaction was
    // retired underneath it roll back the NEXT transaction's session.
    if !crate::tx_lanes::with(|l| identity.is_current(l, app_id)) {
        return CleanupAck::Indeterminate;
    }
    rollback_session_in_slot(app_id)
        .await
        .unwrap_or(CleanupAck::Indeterminate)
}

// ---------------------------------------------------------------------------
// Session disposition
// ---------------------------------------------------------------------------

/// [`Action::ReleaseSession`]: hand the session back.
///
/// On PostgreSQL that is a plain drop, whose `Drop` returns the lease to the
/// pool. This is the ONLY disposition that may do that.
fn release_session(app_id: &str) {
    let client = crate::tx_lanes::with_mut(|l| {
        // BEFORE the drop, and load-bearing. `OwnedPooledClient::drop` returns
        // the lease, and `Pool::return_client` RETIRES any session whose cancel
        // lease has escaped (`Arc::strong_count(lease) > 1`). A canceller still
        // parked here holds exactly such a reference, so leaving it would
        // destroy the connection on the ordinary success path - the opposite of
        // what capturing it is for. See
        // `ThreadDbContext::remove_tx_canceller`.
        l.remove_tx_canceller(app_id);
        l.take_tx_client_for(app_id)
    });
    drop(client);
}

/// [`Action::WithdrawSession`]: destroy the physical connection.
///
/// **A drop is not a withdrawal.** `OwnedPooledClient::drop` calls
/// `pool.return_client(entry)`, which republishes the lease as idle - so the
/// next borrower inherits precisely the session SC-1 withdrew. Closing the
/// client's request channel first makes `PoolEntry::is_pool_eligible` false
/// (it checks `!client.is_closed()`), and `return_client` then evicts the entry
/// and releases its capacity slot instead of publishing it.
///
/// [`crate::context::ThreadDbContext::withdraw_tx_session`] also sets a
/// per-app tombstone, so a session another future is holding out of the slot is
/// destroyed when that future returns it rather than quietly parked.
fn destroy_session(app_id: &str) {
    let client = crate::tx_lanes::with_mut(|l| {
        // Symmetric with `release_session`, for a different reason: this
        // connection is being destroyed either way, so the escaped-lease rule
        // cannot bite - but a canceller for a session that no longer exists is
        // a handle to nothing, and leaving it would make the map's contents a
        // weaker statement than "these sessions are live and cancellable".
        l.remove_tx_canceller(app_id);
        l.withdraw_tx_session(app_id)
    });
    if let Some(client) = client {
        crate::tx_lanes::destroy_tx_connection(client);
    }
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// The execution budget a transaction is admitted under.
///
/// `execution` matches the DB-1 `idle_in_transaction_session_timeout` the
/// session already carries, so the protocol deadline and the server-side guard
/// bound the same window rather than two different ones. The two cleanup
/// budgets are the grace SC-1 gives a backend that owes an answer; there is no
/// third timer and no escalation past them - the session is withdrawn.
fn budgets() -> TxBudgets {
    TxBudgets {
        // From `zeroship_data_core::budgets`, not from `auth::bootstrap`: this deadline binds
        // the SQLite arm too, and reaching for it through the PostgreSQL
        // session-setup module is what made a cross-backend policy number look
        // like a PostgreSQL detail.
        execution: Duration::from_millis(u64::from(zeroship_data_core::budgets::DB_IDLE_IN_TX_TIMEOUT_MS)),
        cancellation_sql: Duration::from_secs(5),
        terminal_sql: Duration::from_secs(10),
    }
}

/// Spawn the timer [`Action::ScheduleTimer`] asks for.
///
/// The task carries the app key, the kind and the generation, and nothing else -
/// no session, no client, no settle future. A stale delivery is a pure
/// diagnostic: the reducer's slot refuses any `(kind, generation)` pair it is
/// not holding, and refusing produces no SQL, no reply and no state change.
///
/// ## Known cost, deliberately not paid down here
///
/// The task is detached and sleeps for the whole budget, so a transaction that
/// commits in milliseconds leaves one asleep for the rest of its execution
/// budget. The late fire is harmless - see above - but the accumulation scales
/// with transaction RATE rather than with concurrency, which is the shape that
/// bites a busy worker.
///
/// The fix is not cheap and does not belong in a change about cancellation.
/// Cancelling the sleep needs a signal, and the signal has to be owned by
/// whoever disarms the deadline. That is
/// [`super::reducer::deadline::DeadlineSlot`], which lives in the PURE reducer
/// and may not own an I/O handle. So it needs a new `Action` for "cancel the
/// timer you scheduled", a driver-side registry keyed the same way the slot is,
/// and arms proving a cancelled timer cannot take a live `(kind, generation)`
/// with it. That is a self-contained change with its own tests, not a rider on
/// this one.
fn schedule_timer(app_id: &str, scheduled: super::reducer::deadline::ScheduleTimer) {
    let app_id = app_id.to_string();
    compio::runtime::spawn(async move {
        let delay = scheduled.at.saturating_duration_since(Instant::now());
        compio::time::sleep(delay).await;
        let _ = deadline_fired(&app_id, scheduled.kind, scheduled.generation).await;
    })
    .detach();
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Lower a protocol refusal to the creator-visible error.
///
/// Every refusal carries its own code - `TxProtocolError::code()` - so a caller
/// can branch on what was refused rather than on a collapsed message. The
/// backend error, when there is one, supplies the detail.
pub(crate) fn protocol_error(refusal: TxProtocolError, detail: Option<DbError>) -> DbError {
    let message = detail.as_ref().map_or_else(
        || format!("db.transaction: {}", refusal.code()),
        |error| {
            format!(
                "db.transaction: {}: {}",
                refusal.code(),
                error.message_str()
            )
        },
    );
    DbError::Coded {
        code: refusal.code().to_string(),
        message,
        hint: None,
    }
}

/// Lower a terminal outcome to the creator-visible error, or `None` when the
/// transaction ended the way the creator asked.
pub(crate) fn outcome_error(
    outcome: TerminalOutcome,
    intent: SettleIntent,
    detail: Option<DbError>,
) -> Option<DbError> {
    let coded = |code: &str, message: String| DbError::Coded {
        code: code.to_string(),
        message,
        hint: None,
    };
    let detail_text = detail
        .as_ref()
        .map_or_else(String::new, |error| format!(": {}", error.message_str()));
    match (intent, outcome) {
        (SettleIntent::Commit, TerminalOutcome::Committed)
        | (SettleIntent::Rollback, TerminalOutcome::RolledBack) => None,
        // A COMMIT the server answered ROLLBACK. The writes are gone and this
        // must never read as success.
        (SettleIntent::Commit, TerminalOutcome::RolledBack) => Some(coded(
            "commit_rolled_back",
            format!(
                "commit failed - PostgreSQL rolled the transaction back and its \
                 writes were discarded{detail_text}"
            ),
        )),
        (_, TerminalOutcome::Indeterminate(cause)) => Some(coded(
            "commit_failed_indeterminate",
            format!(
                "transaction state indeterminate ({}){detail_text}",
                cause.code()
            ),
        )),
        (_, TerminalOutcome::Cancelled(cause)) => Some(coded(
            cause.code(),
            format!("db.transaction: {}{detail_text}", cause.code()),
        )),
        (_, TerminalOutcome::ResultMismatch) => Some(coded(
            "settle_result_mismatch",
            format!("the backend's terminal answer contradicts the request{detail_text}"),
        )),
        (SettleIntent::Rollback, TerminalOutcome::Committed) => Some(coded(
            "settle_result_mismatch",
            format!("a rollback was answered with a commit{detail_text}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::next_backend_generation;

    /// The generation sequence must never restart.
    ///
    /// A counter that restarted would let a stale completion authenticate
    /// against a later session by arithmetic coincidence, which is exactly what
    /// SC-1's guard order step 4 exists to refuse.
    ///
    /// It reads the REAL thread-local rather than a private counter, which the
    /// version on `ThreadDbContext` could not: that one built its own struct, so
    /// it proved the arithmetic and never touched the store the driver uses.
    /// Safe because libtest gives every `#[test]` its own OS thread even under
    /// `--test-threads=1`, so no other test shares this sequence.
    #[test]
    fn backend_generations_are_monotonic() {
        let first = next_backend_generation();
        let second = next_backend_generation();
        assert!(second > first, "generations must strictly increase");
        assert!(
            next_backend_generation() > second,
            "the generation sequence must keep increasing"
        );
    }

    /// `reset_context_for_tests` must NOT restart the sequence.
    ///
    /// The counter used to live on `ThreadDbContext`, where that helper rebuilt
    /// the whole struct and silently returned it to 0 - contradicting the
    /// "never reset" the field's own doc claimed. This pins the corrected
    /// behaviour, and would fail against the pre-2026-09-02 placement.
    #[test]
    fn a_context_reset_does_not_restart_the_generation_sequence() {
        let before = next_backend_generation();
        crate::reset_context_for_tests();
        assert!(
            next_backend_generation() > before,
            "a context reset must not rewind the backend generation: a stale \
             completion could then authenticate against a later session"
        );
    }
}

