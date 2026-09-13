//! Transaction reducer state-transition tests.

use std::time::{Duration, Instant};

use super::deadline::{DeadlineKind, DeadlineState};
use super::frames::{Effect, FrameClose};
use super::identity::{
    AuthorityDomain, AuthorityIdentity, ExpectedAuthority, LifecycleState, MaskCeiling,
    ObservedAuthority, SchemaEpoch,
};
use super::*;

const MAX_DEPTH: u32 = crate::transaction::MAX_SAVEPOINT_DEPTH;

fn domain() -> AuthorityDomain {
    AuthorityDomain::new(7_262_000_000_000_000_001, 1)
}

fn identity() -> AuthorityIdentity {
    AuthorityIdentity::for_app("app_alpha", 4)
}

fn expected() -> ExpectedAuthority {
    ExpectedAuthority {
        identity: identity(),
        domain: domain(),
        epoch: SchemaEpoch::new(11),
    }
}

fn authority() -> EventAuthority {
    EventAuthority {
        identity: identity(),
        domain: domain(),
    }
}

/// A `Current`-classifying observation, boxed because that is the shape
/// [`TxEvent::AuthorityObserved`] carries.
///
/// The `Box` is not incidental: `ObservedAuthority` owns a `MaskCeiling` set and
/// two heap identities, and `TxEvent` is matched on every event, so boxing the
/// largest variant keeps the enum small. Returning it boxed here means every
/// arm builds the event the way production does.
#[allow(
    clippy::unnecessary_box_returns,
    reason = "mirrors the boxed field TxEvent::AuthorityObserved actually carries"
)]
fn current_observation() -> Box<ObservedAuthority> {
    Box::new(ObservedAuthority {
        identity: identity(),
        domain: domain(),
        epoch: SchemaEpoch::new(11),
        lifecycle: LifecycleState::Stable,
        ceiling: MaskCeiling::of(["support", "auto"]),
    })
}

/// A driver that records every action, so an arm can rule on what reached the
/// wire rather than only on the state that resulted.
struct Harness {
    reducer: TxReducer,
    now: Instant,
    emitted: Vec<Action>,
}

impl Harness {
    /// Admit a transaction. The reducer is in `Preparing` and the execution
    /// deadline is armed.
    fn admit() -> Self {
        let now = Instant::now();
        let (reducer, actions) = TxReducer::admit(expected(), TxBudgets::default(), now, MAX_DEPTH);
        Self {
            reducer,
            now,
            emitted: actions,
        }
    }

    fn apply(&mut self, event: TxEvent) -> Vec<Action> {
        let actions = self.reducer.apply(event, self.now);
        self.emitted.extend(actions.iter().cloned());
        actions
    }

    fn state(&self) -> TxState {
        self.reducer.state()
    }

    /// Every action this harness has ever seen.
    fn all(&self) -> &[Action] {
        &self.emitted
    }

    /// The token of the single outstanding command, read out of the last
    /// action that minted one.
    fn last_token(&self) -> CommandToken {
        self.emitted
            .iter()
            .rev()
            .find_map(|action| match action {
                Action::IssueBegin { token }
                | Action::IssueDataSql { token }
                | Action::IssueSavepoint { token, .. }
                | Action::IssueRollbackTo { token, .. }
                | Action::IssueRelease { token, .. }
                | Action::IssueTerminal { token, .. }
                | Action::IssueCancellation { token, .. } => Some(*token),
                _ => None,
            })
            .expect("no command has been issued")
    }

    /// Advance to `Starting` - the authority read returned `Current`.
    fn advance_to_starting(mut self) -> Self {
        self.apply(TxEvent::AuthorityObserved {
            authority: authority(),
            observed: current_observation(),
        });
        assert_eq!(self.state(), TxState::Starting);
        self
    }

    /// Advance to `Idle` - `BEGIN` confirmed.
    fn advance_to_idle(mut self) -> Self {
        self = self.advance_to_starting();
        let token = self.last_token();
        self.apply(TxEvent::BeginCompleted {
            token,
            outcome: BeginOutcome::Opened(BackendGeneration(1)),
        });
        assert_eq!(self.state(), TxState::Idle);
        self
    }

    /// Advance to `InFlight` - a creator statement owns the client.
    fn advance_to_in_flight(mut self) -> Self {
        self = self.advance_to_idle();
        self.apply(TxEvent::OperationRequested);
        assert_eq!(self.state(), TxState::InFlight);
        self
    }

    /// Advance to `Quiescing` - a settle arrived while a command owns
    /// execution.
    fn advance_to_quiescing(mut self) -> Self {
        self = self.advance_to_in_flight();
        self.apply(TxEvent::SettleRequested {
            intent: SettleIntent::Commit,
        });
        assert_eq!(self.state(), TxState::Quiescing);
        self
    }

    /// Advance to `Poisoned` - a statement errored.
    fn advance_to_poisoned(mut self) -> Self {
        self = self.advance_to_in_flight();
        let token = self.last_token();
        self.apply(TxEvent::OperationCompleted {
            token,
            errored: true,
        });
        assert_eq!(self.state(), TxState::Poisoned);
        self
    }

    /// Drive to one of the six forceable states.
    fn at(state: TxState) -> Self {
        match state {
            TxState::Preparing => Self::admit(),
            TxState::Starting => Self::admit().advance_to_starting(),
            TxState::Idle => Self::admit().advance_to_idle(),
            TxState::InFlight => Self::admit().advance_to_in_flight(),
            TxState::Quiescing => Self::admit().advance_to_quiescing(),
            TxState::Poisoned => Self::admit().advance_to_poisoned(),
            other => panic!("{other:?} is not a forceable state"),
        }
    }

    /// Open a child frame and confirm its `SAVEPOINT`. Requires `Idle`.
    fn with_open_child(mut self) -> (Self, FrameId) {
        let actions = self.apply(TxEvent::OpenFrame);
        let frame = self
            .reducer
            .frames()
            .top()
            .expect("a child was inserted")
            .id();
        let token = match actions.first() {
            Some(Action::IssueSavepoint { token, .. }) => *token,
            other => panic!("expected IssueSavepoint, got {other:?}"),
        };
        self.apply(TxEvent::OpenFrameCompleted {
            token,
            frame,
            ok: true,
        });
        assert_eq!(self.state(), TxState::Idle);
        (self, frame)
    }
}

// ---------------------------------------------------------------------------
// THE forbidden route. The single most important property of this change.
// ---------------------------------------------------------------------------

/// **Forced cleanup never routes through `Poisoned`.**
///
/// Drives a force from every one of the six forceable states and asserts the
/// result is `Cancelling` in all six. Driving `TxState::FORCEABLE` rather than
/// a remembered list is what makes the arm rule on the whole set: a seventh
/// forceable state added later is covered without anyone editing this test.
///
/// Where this would fail today: on any implementation that parks a force in
/// `Poisoned` - which is the shape SC-1 previously described, and which passes
/// every other arm in this file except this one and
/// `a_forced_transaction_cannot_be_resurrected_by_rollback_to`. `Poisoned` is
/// the single nonterminal state invariant 13 lets a creator command walk back
/// to `Idle`, so routing a fired deadline or a `Deny` verdict through it lets
/// the creator's next `rollbackTo` resume data SQL under an authority the
/// classifier terminally denied.
///
/// What this fixture deliberately does not cover: the resurrection itself.
/// That needs an open child frame to roll back to, and is the arm below.
#[test]
fn a_force_never_routes_through_poisoned() {
    for state in TxState::FORCEABLE {
        let mut harness = Harness::at(state);
        harness.apply(TxEvent::Cancel {
            authority: authority(),
        });
        assert_eq!(
            harness.state(),
            TxState::Cancelling,
            "a force from {state:?} must enter Cancelling, never Poisoned and never Settling"
        );
        assert_ne!(harness.state(), TxState::Poisoned);
        assert_eq!(
            harness.reducer.cleanup().map(|latched| latched.cause),
            Some(CleanupCause::Cancelled)
        );
    }
    assert_eq!(
        TxState::FORCEABLE.len(),
        6,
        "the six states SC-1 names; a change to the set must reach this arm"
    );
}

/// **A forced transaction cannot be resurrected by `rollbackTo`.**
///
/// The discriminating arm for `Cancelling`. A force fires while an open child
/// frame exists, then the creator callback issues `rollbackTo` on that child -
/// which is precisely invariant 13's recovery arc, the one that returns a
/// `Poisoned` parent to `Idle`.
///
/// Where this would fail today: on an implementation that routes forced
/// cleanup through `Poisoned`. There the `rollbackTo` below is legal, succeeds,
/// and returns the transaction to `Idle`, from which data SQL resumes - past a
/// deadline that already fired. It asserts the **cause the caller receives**,
/// not merely that the transaction eventually ended: a rollback that ends the
/// transaction for the wrong reason is what the single-latched cause exists to
/// prevent.
#[test]
fn a_forced_transaction_cannot_be_resurrected_by_rollback_to() {
    let (mut harness, child) = Harness::at(TxState::Idle).with_open_child();

    // The execution deadline is the cheapest force.
    let generation = match harness.reducer.deadline().state() {
        DeadlineState::Armed {
            kind: DeadlineKind::Execution,
            generation,
            ..
        } => generation,
        other => panic!("expected an armed Execution deadline, got {other:?}"),
    };
    harness.apply(TxEvent::DeadlineFired {
        kind: DeadlineKind::Execution,
        generation,
    });

    // **No state assertion here, deliberately.** Asserting `Cancelling` at this
    // point would make this arm fail at the same place
    // `a_force_never_routes_through_poisoned` already does, and the
    // resurrection below - the thing this arm is named for - would never be
    // reached under the very implementation it exists to catch. That is
    // verification-record class 7: the fixture would not reach the claim.
    let before = harness.all().len();
    let replies = harness.apply(TxEvent::CloseFrame {
        frame: child,
        close: FrameClose::RolledBackTo,
    });

    assert_eq!(
        replies,
        vec![Action::Reply(Err(TxProtocolError::Cleanup(
            CleanupCause::DeadlineExpired(DeadlineKind::Execution)
        )))],
        "the caller receives the latched cleanup cause, not a generic refusal \
         and not a success - a rollback that ends the transaction for the \
         WRONG reason is what the single-latched cause exists to prevent"
    );
    assert!(
        !harness.all()[before..]
            .iter()
            .any(Action::is_creator_data_sql),
        "invariant 16: no creator data SQL is ever issued again after a force"
    );

    // The resurrection itself: the recovery arc must not complete.
    assert_ne!(
        harness.state(),
        TxState::Idle,
        "a forced transaction walked back to Idle would resume data SQL under \
         an authority the classifier denied, past a deadline that already fired"
    );
    assert_eq!(harness.state(), TxState::Cancelling);

    // And a following data statement is refused for the same cause.
    let replies = harness.apply(TxEvent::OperationRequested);
    assert_eq!(
        replies,
        vec![Action::Reply(Err(TxProtocolError::Cleanup(
            CleanupCause::DeadlineExpired(DeadlineKind::Execution)
        )))]
    );
}

/// **The completion of the command a force interrupted must not move the
/// state.**
///
/// This interleaving became reachable when forced cleanup started CANCELLING a
/// command-owned session instead of withdrawing it. Before that, a force landing
/// on a command-owned session proved no goal and reached `Settled` inside the
/// same `apply`, so the command's completion always arrived at a `Settled`
/// reducer and guard 5 answered it. Cancelling means the cleanup waits for the
/// cancelled statement, and its completion now arrives here, in `Cancelling`.
///
/// Every one of the three completion handlers would otherwise assign a state:
/// `on_operation_completed` writes `Idle` or `Poisoned`, and both frame
/// completions write `Idle` or `Poisoned` too. `Poisoned` is the resurrection
/// route `a_forced_transaction_cannot_be_resurrected_by_rollback_to` covers, and
/// `Idle` is worse - it admits data SQL directly. So a cancelled statement's own
/// error would be the thing that walks a terminally forced transaction back onto
/// the data path.
///
/// The arm drives all three completions rather than the one that is easiest to
/// reach, because the hazard is the handler's shape and all three share it.
///
/// **Mutation that reddens this arm:** delete the `TxState::Cancelling` early
/// return from any one of `on_operation_completed`,
/// `on_open_frame_completed` or `on_close_frame_completed`. That case then
/// reports `Idle` or `Poisoned` instead of `Cancelling`.
#[test]
fn a_cancelled_commands_completion_never_moves_a_forced_transaction() {
    /// A case: how to reach a command that owns the client, and the frame it
    /// names (if any). The `bool` says whether the completion to deliver is a
    /// data statement's or a frame close's.
    type InFlightCase = (&'static str, fn() -> (Harness, Option<FrameId>), bool);

    let cases: [InFlightCase; 3] = [
        (
            "a data statement",
            || (Harness::at(TxState::InFlight), None),
            true,
        ),
        (
            "a SAVEPOINT",
            || {
                let mut harness = Harness::at(TxState::Idle);
                harness.apply(TxEvent::OpenFrame);
                let frame = harness.reducer.frames().top().map(frames::Frame::id);
                (harness, frame)
            },
            false,
        ),
        (
            "a RELEASE",
            || {
                let (mut harness, child) = Harness::at(TxState::Idle).with_open_child();
                harness.apply(TxEvent::CloseFrame {
                    frame: child,
                    close: FrameClose::Released,
                });
                (harness, Some(child))
            },
            false,
        ),
    ];

    for (label, build, is_data_sql) in cases {
        let (mut harness, frame) = build();
        assert_eq!(
            harness.state(),
            TxState::InFlight,
            "{label}: the command must own the client before the force"
        );
        let token = harness.last_token();

        harness.apply(TxEvent::Cancel {
            authority: authority(),
        });
        assert_eq!(
            harness.state(),
            TxState::Cancelling,
            "{label}: the force claims the gate"
        );

        // The cancelled command answers. Its error is real - PostgreSQL
        // reports 57014 - which is exactly the input that would otherwise
        // produce `Poisoned`.
        let before = harness.all().len();
        let replies = if is_data_sql {
            harness.apply(TxEvent::OperationCompleted {
                token,
                errored: true,
            })
        } else {
            harness.apply(TxEvent::CloseFrameCompleted {
                token,
                frame: frame.expect("a frame case names its frame"),
                close: FrameClose::Released,
                ok: false,
            })
        };

        assert_eq!(
            harness.state(),
            TxState::Cancelling,
            "{label}: the completion of a cancelled command must not move a \
             forced transaction - Idle admits data SQL and Poisoned is \
             recoverable to Idle by rollbackTo"
        );
        assert_eq!(
            replies,
            vec![Action::Reply(Err(TxProtocolError::Cleanup(
                CleanupCause::Cancelled
            )))],
            "{label}: the caller receives the latched cleanup cause"
        );
        assert!(
            !harness.all()[before..]
                .iter()
                .any(Action::is_creator_data_sql),
            "{label}: invariant 16 - no creator data SQL after a force"
        );
        // The session is handed back to the registry, because that is now
        // true: the guard that was holding it has returned it, and the
        // cleanup ROLLBACK is what runs next.
        assert_eq!(
            harness.reducer.session(),
            SessionOwnership::Registry,
            "{label}: a returned session is the registry's again, so cleanup \
             can reach it"
        );
    }
}

/// The `OpenFrameCompleted` half of the arm above.
///
/// Split out rather than folded in because `OpenFrameCompleted` carries a
/// different event shape, and a loop that special-cased it would be a loop with
/// one arm per case wearing a table's clothes.
///
/// **Mutation that reddens this arm:** delete the `TxState::Cancelling` early
/// return from `on_open_frame_completed`. The state becomes `Poisoned`.
#[test]
fn a_cancelled_savepoint_completion_never_moves_a_forced_transaction() {
    let mut harness = Harness::at(TxState::Idle);
    harness.apply(TxEvent::OpenFrame);
    let frame = harness
        .reducer
        .frames()
        .top()
        .expect("a child was inserted")
        .id();
    let token = harness.last_token();
    assert_eq!(harness.state(), TxState::InFlight);

    harness.apply(TxEvent::Cancel {
        authority: authority(),
    });
    assert_eq!(harness.state(), TxState::Cancelling);

    let replies = harness.apply(TxEvent::OpenFrameCompleted {
        token,
        frame,
        ok: false,
    });
    assert_eq!(
        harness.state(),
        TxState::Cancelling,
        "a cancelled SAVEPOINT's failure must not park a forced transaction in \
         Poisoned, from which rollbackTo walks it back to Idle"
    );
    assert_eq!(
        replies,
        vec![Action::Reply(Err(TxProtocolError::Cleanup(
            CleanupCause::Cancelled
        )))]
    );
}

/// Invariant 16 over interleavings: `Cancelling` has no exit but `Settled`,
/// and exactly one cleanup cause is ever latched.
///
/// Where this would fail today: on an implementation whose gate is
/// last-writer-wins, which lets the second force overwrite the first and makes
/// the reason a transaction ended nondeterministic - the reason being exactly
/// what the creator is told.
#[test]
fn cancelling_latches_exactly_one_cause_and_every_exit_is_settled() {
    for state in TxState::FORCEABLE {
        let mut harness = Harness::at(state);
        harness.apply(TxEvent::Cancel {
            authority: authority(),
        });
        let latched = harness.reducer.cleanup().expect("a cause is latched");
        assert_eq!(latched.cause, CleanupCause::Cancelled);

        // A second, different force joins and emits nothing.
        let actions = harness.apply(TxEvent::DetachRequested {
            authority: authority(),
        });
        assert!(actions.is_empty(), "a joined force emits nothing");
        assert_eq!(
            harness.reducer.cleanup(),
            Some(latched),
            "exactly one cleanup cause is ever latched"
        );

        // The only exit is Settled.
        let token = harness.last_token();
        harness.apply(TxEvent::CancellationAcknowledged {
            token,
            ack: match latched.goal {
                CleanupGoal::NoTransaction => CleanupAck::NoOpenTransaction,
                _ => CleanupAck::RolledBack,
            },
        });
        assert_eq!(harness.state(), TxState::Settled);
        assert_eq!(
            harness.reducer.outcome(),
            Some(TerminalOutcome::Cancelled(CleanupCause::Cancelled))
        );
    }
}

// ---------------------------------------------------------------------------
// Preparing: a fired execution deadline where no BEGIN was ever sent
// ---------------------------------------------------------------------------

/// **A fired execution deadline in `Preparing` sends no `BEGIN`.**
///
/// The fixture fires the deadline while the platform-role authority read is
/// still in flight, so the reducer has never seen a `Current` verdict and has
/// never issued `BEGIN`. The cleanup goal must therefore be `NoTransaction`,
/// and no `IssueBegin` may appear anywhere in the action log.
///
/// Where this would fail today: on an implementation that issues `BEGIN`
/// before the authority read returns - the "early `Starting`" shape SC-1 says
/// a five-state list has to fake, which opens a transaction on an authority
/// nobody checked. It also fails on one that fixes the cleanup goal from the
/// state at *acknowledgement* rather than at entry, which would read
/// `Cancelling` and demand `OpenTransaction` for a transaction that was never
/// opened.
///
/// What this fixture deliberately does not cover: a force in `Starting`, where
/// `BEGIN` is in flight and the goal is `AbortIfOpened`. That is the arm
/// below, and it is the one case where the answer is genuinely unknown.
#[test]
fn a_fired_execution_deadline_in_preparing_sends_no_begin() {
    let mut harness = Harness::admit();
    assert_eq!(harness.state(), TxState::Preparing);

    let generation = match harness.reducer.deadline().state() {
        DeadlineState::Armed {
            kind: DeadlineKind::Execution,
            generation,
            ..
        } => generation,
        other => panic!("the execution deadline is armed on entry to Preparing, got {other:?}"),
    };
    harness.apply(TxEvent::DeadlineFired {
        kind: DeadlineKind::Execution,
        generation,
    });

    assert_eq!(harness.state(), TxState::Cancelling);
    assert_eq!(
        harness.reducer.cleanup().map(|latched| latched.goal),
        Some(CleanupGoal::NoTransaction),
        "BEGIN was never sent, so the goal is proved by 'no open transaction'"
    );
    assert!(
        !harness
            .all()
            .iter()
            .any(|action| matches!(action, Action::IssueBegin { .. })),
        "no BEGIN may be issued from Preparing under any path"
    );

    // The admission claim is held until the acknowledgement arrives - SC-2
    // case 1: "SC-1 releases its admission only after reducing that proof".
    assert!(
        !harness
            .all()
            .iter()
            .any(|action| matches!(action, Action::ReleaseAdmission)),
        "the claim is not released before the backend retires the reservation"
    );

    let token = harness.last_token();
    harness.apply(TxEvent::CancellationAcknowledged {
        token,
        ack: CleanupAck::NoOpenTransaction,
    });
    assert_eq!(harness.state(), TxState::Settled);
    assert!(
        harness
            .all()
            .iter()
            .any(|action| matches!(action, Action::ReleaseAdmission)),
        "invariant 3: every granted admission has exactly one release"
    );
}

/// A force in `Starting` cannot know whether a transaction exists to end, so
/// its goal is `AbortIfOpened` and **either** acknowledgement proves it.
///
/// Where this would fail today: on an implementation that demands
/// `OpenTransaction` after a force in `Starting`, which withdraws a healthy
/// connection every time the `BEGIN` had not landed.
#[test]
fn a_force_in_starting_accepts_either_acknowledgement() {
    for ack in [CleanupAck::NoOpenTransaction, CleanupAck::RolledBack] {
        let mut harness = Harness::at(TxState::Starting);
        harness.apply(TxEvent::Cancel {
            authority: authority(),
        });
        assert_eq!(
            harness.reducer.cleanup().map(|latched| latched.goal),
            Some(CleanupGoal::AbortIfOpened)
        );
        let token = harness.last_token();
        harness.apply(TxEvent::CancellationAcknowledged { token, ack });
        assert_eq!(
            harness.reducer.outcome(),
            Some(TerminalOutcome::Cancelled(CleanupCause::Cancelled)),
            "{ack:?} proves AbortIfOpened"
        );
        assert_eq!(
            harness.reducer.session(),
            SessionOwnership::None,
            "a proved goal returns the session rather than withdrawing it"
        );
    }
}

/// An acknowledgement that contradicts the goal, or one that is
/// indeterminate, settles as indeterminate and **withdraws** the session.
///
/// Where this would fail today: on an implementation that returns the
/// connection to the pool after unproved cleanup, which is the failure the
/// driver already names - handing the next user of a connection an aborted
/// transaction.
#[test]
fn an_unproved_cleanup_withdraws_the_session_rather_than_returning_it() {
    // `OpenTransaction` is contradicted by "no open transaction" and unproved
    // by "indeterminate". Both take the same disposition.
    for ack in [CleanupAck::NoOpenTransaction, CleanupAck::Indeterminate] {
        let mut harness = Harness::at(TxState::Idle);
        harness.apply(TxEvent::Cancel {
            authority: authority(),
        });
        assert_eq!(
            harness.reducer.cleanup().map(|latched| latched.goal),
            Some(CleanupGoal::OpenTransaction)
        );
        let token = harness.last_token();
        harness.apply(TxEvent::CancellationAcknowledged { token, ack });
        assert_eq!(
            harness.reducer.outcome(),
            Some(TerminalOutcome::Indeterminate(CleanupCause::Cancelled)),
            "{ack:?} does not prove OpenTransaction"
        );
        assert_eq!(harness.reducer.session(), SessionOwnership::Withdrawn);
        assert!(harness.all().contains(&Action::WithdrawSession));
        assert!(!harness.all().contains(&Action::ReleaseSession));
    }
}

// ---------------------------------------------------------------------------
// Quiescing: a settle requested while a command still owns execution
// ---------------------------------------------------------------------------

/// **A settle arriving while an operation owns the client issues ZERO SQL**,
/// then issues terminal SQL exactly once when that operation returns.
///
/// Where this would fail today: on the shipped settle path, which is DBR-03.
/// `take_tx_client_for` returning `None` falls into "Slot already drained ...
/// Treat as settled", releases the claim, clears pending emits and returns
/// `SettleOutcome::Ok` **without sending anything**
/// (`transaction/mod.rs:1034-1039`). This arm asserts the opposite of that on
/// both halves: nothing is sent while the command is out, and exactly one
/// terminal statement is sent after it returns.
#[test]
fn a_settle_while_a_command_owns_execution_quiesces_and_issues_no_sql() {
    let mut harness = Harness::at(TxState::InFlight);
    let operation = harness.last_token();

    let before = harness.all().len();
    let actions = harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Commit,
    });
    assert_eq!(harness.state(), TxState::Quiescing);
    assert!(
        actions.is_empty(),
        "invariant 15: zero frame or terminal SQL until the active command returns"
    );
    assert!(
        !harness.all()[before..].iter().any(|action| matches!(
            action,
            Action::IssueTerminal { .. }
                | Action::IssueRelease { .. }
                | Action::IssueRollbackTo { .. }
        )),
        "an absent client is never proof that terminal SQL ran"
    );

    // The command returns; now exactly one terminal statement is issued.
    let after = harness.all().len();
    harness.apply(TxEvent::OperationCompleted {
        token: operation,
        errored: false,
    });
    assert_eq!(harness.state(), TxState::Settling);
    let terminals = harness.all()[after..]
        .iter()
        .filter(|action| matches!(action, Action::IssueTerminal { .. }))
        .count();
    assert_eq!(
        terminals, 1,
        "terminal SQL is sent exactly once, and only after the command returned"
    );
}

/// `Quiescing` latches exactly one intent: a second request naming the same
/// attempt joins, a different one is a settle conflict.
///
/// Where this would fail today: on an implementation that lets a second
/// settle overwrite the latched intent, which turns a creator's commit into a
/// rollback (or the reverse) depending on arrival order.
#[test]
fn quiescing_latches_exactly_one_intent() {
    let mut harness = Harness::at(TxState::Quiescing);

    let same = harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Commit,
    });
    assert!(
        same.is_empty(),
        "the same attempt joins the waiter already there"
    );

    let different = harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Rollback,
    });
    assert_eq!(
        different,
        vec![Action::Reply(Err(TxProtocolError::SettleConflict))]
    );
    assert_eq!(harness.state(), TxState::Quiescing);

    // The latched intent is still the first one.
    let operation = harness.last_token();
    harness.apply(TxEvent::OperationCompleted {
        token: operation,
        errored: false,
    });
    assert!(harness.all().iter().any(|action| matches!(
        action,
        Action::IssueTerminal {
            intent: SettleIntent::Commit,
            ..
        }
    )));
}

/// A force arriving in `Quiescing` cannot wait for the command to return - the
/// deadline fired *because* it is not returning - so it enters `Cancelling`
/// without issuing the latched terminal SQL.
///
/// Where this would fail today: on an implementation that treats a force in
/// `Quiescing` as "run the latched settle now", which issues terminal SQL on a
/// session a command still owns and breaks invariant 5.
#[test]
fn a_force_in_quiescing_does_not_issue_the_latched_terminal_sql() {
    let mut harness = Harness::at(TxState::Quiescing);
    let before = harness.all().len();
    harness.apply(TxEvent::Cancel {
        authority: authority(),
    });
    assert_eq!(harness.state(), TxState::Cancelling);
    assert!(
        !harness.all()[before..]
            .iter()
            .any(|action| matches!(action, Action::IssueTerminal { .. })),
        "a force must not issue terminal SQL on a session a command owns"
    );
    assert_eq!(
        harness.reducer.cleanup().map(|latched| latched.goal),
        Some(CleanupGoal::OpenTransaction)
    );
}

// ---------------------------------------------------------------------------
// The second-stage deadline
// ---------------------------------------------------------------------------

/// **The second-stage deadline settles rather than hanging.**
///
/// The forced rollback issued from `Cancelling` never answers, so the
/// `CancellationSql` deadline must fire, the session must be withdrawn rather
/// than returned, the transaction must reach `Settled` with an indeterminate
/// outcome, and the admission claim must be released so a following
/// transaction for the same app can be admitted.
///
/// Where this would fail today: on an implementation without the second-stage
/// deadline. This is invariant 3's claim balance on the path that previously
/// had nothing to release the claim - so such an implementation **hangs** this
/// arm rather than failing it. The reducer is synchronous, so the hang becomes
/// a missing `ReleaseAdmission` rather than a wall-clock stall, which is why
/// this arm can assert it directly.
#[test]
fn the_second_stage_deadline_settles_rather_than_hanging() {
    let mut harness = Harness::at(TxState::Idle);
    harness.apply(TxEvent::Cancel {
        authority: authority(),
    });
    assert_eq!(harness.state(), TxState::Cancelling);

    // The cleanup deadline replaced the execution one.
    let generation = match harness.reducer.deadline().state() {
        DeadlineState::Armed {
            kind: DeadlineKind::CancellationSql,
            generation,
            ..
        } => generation,
        other => panic!("Cancelling must be bounded by CancellationSql, got {other:?}"),
    };

    // The backend never answers; the deadline fires.
    harness.apply(TxEvent::DeadlineFired {
        kind: DeadlineKind::CancellationSql,
        generation,
    });

    assert_eq!(harness.state(), TxState::Settled);
    assert_eq!(
        harness.reducer.outcome(),
        Some(TerminalOutcome::Indeterminate(CleanupCause::Cancelled)),
        "the latched cause is carried, not replaced by the deadline"
    );
    assert_eq!(harness.reducer.session(), SessionOwnership::Withdrawn);
    assert!(harness.all().contains(&Action::WithdrawSession));
    assert!(
        harness.all().contains(&Action::ReleaseAdmission),
        "the claim is released so a following transaction is admitted"
    );
    assert_eq!(
        harness.reducer.deadline().state(),
        DeadlineState::Disarmed,
        "terminal cleanup leaves no armed timer behind"
    );
}

/// Guard order step 3: a fired deadline runs a **pure** capability check
/// before claiming the timer, so an illegal cell cannot claim a timer as a
/// side effect of being rejected.
///
/// Where this would fail today: on an implementation that claims first and
/// checks afterwards. There the rejected `TerminalSql` delivery below consumes
/// the arming, and the live `CancellationSql` deadline can never fire - which
/// is the hang the arm above exists to prevent, reached by a different route.
#[test]
fn an_illegal_deadline_delivery_does_not_consume_the_arming() {
    let mut harness = Harness::at(TxState::Idle);
    harness.apply(TxEvent::Cancel {
        authority: authority(),
    });
    let armed = harness.reducer.deadline().state();
    assert!(matches!(
        armed,
        DeadlineState::Armed {
            kind: DeadlineKind::CancellationSql,
            ..
        }
    ));

    // A `TerminalSql` fire is not preflight-legal in `Cancelling`.
    let generation = match armed {
        DeadlineState::Armed { generation, .. } => generation,
        other => panic!("expected Armed, got {other:?}"),
    };
    let actions = harness.apply(TxEvent::DeadlineFired {
        kind: DeadlineKind::TerminalSql,
        generation,
    });
    assert!(actions.is_empty(), "a pure diagnostic emits nothing");
    assert_eq!(
        harness.reducer.deadline().state(),
        armed,
        "the rejected delivery must not consume the live arming"
    );

    // The legal one still works.
    harness.apply(TxEvent::DeadlineFired {
        kind: DeadlineKind::CancellationSql,
        generation,
    });
    assert_eq!(harness.state(), TxState::Settled);
}

// ---------------------------------------------------------------------------
// The classifier, driven through the reducer
// ---------------------------------------------------------------------------

/// A `Deny` verdict forces cleanup and reaches the caller as the **specific**
/// denial, and it never issues `BEGIN`.
///
/// Where this would fail today: on an implementation that collapses the three
/// authority-observation reasons, or that parks a denial in `Poisoned` (from
/// which invariant 13's recovery arc resumes data SQL under an authority the
/// classifier terminally denied). Driving `DenyReason::AUTHORITY` is what makes
/// it rule on that classifier's whole output set; setup denials enter later.
#[test]
fn a_deny_verdict_forces_cleanup_and_carries_its_specific_reason() {
    let cases = [
        (
            DenyReason::AppDeprovisioned,
            ObservedAuthority {
                lifecycle: LifecycleState::Deprovisioned,
                ..*current_observation()
            },
        ),
        (
            DenyReason::StaleAppIncarnation,
            ObservedAuthority {
                identity: AuthorityIdentity::for_app("app_alpha", 5),
                ..*current_observation()
            },
        ),
        (
            DenyReason::AuthorityDomainMismatch,
            ObservedAuthority {
                domain: AuthorityDomain::new(7_262_000_000_000_000_002, 1),
                ..*current_observation()
            },
        ),
    ];
    assert_eq!(
        cases.len(),
        DenyReason::AUTHORITY.len(),
        "every authority-observation denial reason has a case here"
    );

    for (reason, observed) in cases {
        let mut harness = Harness::admit();
        let actions = harness.apply(TxEvent::AuthorityObserved {
            authority: authority(),
            observed: Box::new(observed),
        });
        assert_eq!(
            harness.state(),
            TxState::Cancelling,
            "{reason} must force cleanup, never Poisoned"
        );
        assert!(
            actions.contains(&Action::Reply(Err(TxProtocolError::Denied(reason)))),
            "the caller receives {reason}, never a collapsed error"
        );
        assert!(
            !harness
                .all()
                .iter()
                .any(|action| matches!(action, Action::IssueBegin { .. })),
            "a denied transaction issues no BEGIN"
        );
        assert!(!reason.retryable());
    }
}

/// A `ReResolve` verdict is retryable and does not follow the new epoch in
/// place.
///
/// Where this would fail today: on an implementation that adopts the observed
/// epoch and continues, which runs the rest of the transaction against a
/// schema the caller never resolved against.
#[test]
fn a_re_resolve_verdict_is_retryable_and_does_not_follow_the_new_epoch() {
    let mut harness = Harness::admit();
    let actions = harness.apply(TxEvent::AuthorityObserved {
        authority: authority(),
        observed: Box::new(ObservedAuthority {
            epoch: SchemaEpoch::new(12),
            ..*current_observation()
        }),
    });
    assert_eq!(harness.state(), TxState::Cancelling);
    assert!(actions.contains(&Action::Reply(Err(TxProtocolError::EpochChanged))));
    assert!(CleanupCause::EpochChanged.retryable());
    assert!(
        !harness
            .all()
            .iter()
            .any(|action| matches!(action, Action::IssueBegin { .. })),
    );
}

/// The reducer re-runs the classifier on the observation itself, so a
/// publisher cannot smuggle a `Deny` through by labelling it `Current`.
///
/// Where this would fail today: on an implementation that trusts a
/// publisher-attached verdict. The fixture is the strongest form of the
/// attack - the event carries a perfectly routed authority (so guard 1 passes)
/// and an observation that is deprovisioned.
#[test]
fn a_publishers_label_is_an_input_never_a_verdict() {
    let mut harness = Harness::admit();
    // There is no verdict field on the event to lie in - the type makes the
    // attack unrepresentable - so the arm rules on the consequence: an
    // observation that classifies as Deny denies, whatever a publisher wanted.
    harness.apply(TxEvent::AuthorityObserved {
        authority: authority(),
        observed: Box::new(ObservedAuthority {
            lifecycle: LifecycleState::Deprovisioned,
            ..*current_observation()
        }),
    });
    assert_eq!(harness.state(), TxState::Cancelling);
    assert_eq!(
        harness.reducer.cleanup().map(|latched| latched.cause),
        Some(CleanupCause::Denied(DenyReason::AppDeprovisioned))
    );
}

// ---------------------------------------------------------------------------
// Guard order
// ---------------------------------------------------------------------------

/// Guard 1: an event for another authority **must not touch that entry's
/// session, actor, timer or admission** - not even to cancel it.
///
/// Where this would fail today: on an implementation that cancels first and
/// checks identity afterwards, which lets any caller that can name a `TxKey`
/// end another authority's transaction.
#[test]
fn an_event_for_another_authority_touches_nothing() {
    let mut harness = Harness::at(TxState::Idle);
    let state_before = harness.state();
    let deadline_before = harness.reducer.deadline().state();
    let count_before = harness.all().len();

    let foreign = EventAuthority {
        identity: AuthorityIdentity::for_app("app_beta", 4),
        domain: domain(),
    };
    let actions = harness.apply(TxEvent::Cancel {
        authority: foreign.clone(),
    });
    assert_eq!(
        actions,
        vec![Action::Reply(Err(TxProtocolError::AppIncarnationMismatch))]
    );
    assert_eq!(harness.state(), state_before);
    assert_eq!(harness.reducer.deadline().state(), deadline_before);
    assert!(harness.reducer.cleanup().is_none(), "no cause was latched");
    assert_eq!(
        harness.all().len(),
        count_before + 1,
        "the rejection is the only action; nothing was interrupted"
    );

    // A detach for a foreign authority interrupts nothing either.
    let actions = harness.apply(TxEvent::DetachRequested { authority: foreign });
    assert_eq!(
        actions,
        vec![Action::Reply(Err(TxProtocolError::AppIncarnationMismatch))]
    );
    assert_eq!(harness.state(), TxState::Idle);
}

/// Guard 2: a completion carrying a token its current action did not mint
/// changes nothing.
///
/// Where this would fail today: on an implementation that applies a completion
/// by shape rather than by token, which lets a stale reply from a previous
/// statement settle the transaction that replaced it.
#[test]
fn a_stale_completion_token_changes_nothing() {
    let mut harness = Harness::at(TxState::InFlight);
    let stale = CommandToken(9_999);
    let before = harness.state();
    let actions = harness.apply(TxEvent::OperationCompleted {
        token: stale,
        errored: false,
    });
    assert_eq!(
        actions,
        vec![Action::Reply(Err(
            TxProtocolError::StaleTransactionCompletion
        ))]
    );
    assert_eq!(harness.state(), before);
}

/// Invariant 6: a second operation while one is in flight returns
/// `transaction_connection_busy` - never silently deferred, never silently
/// autocommitted.
#[test]
fn a_second_operation_while_one_is_in_flight_is_refused() {
    let mut harness = Harness::at(TxState::InFlight);
    let actions = harness.apply(TxEvent::OperationRequested);
    assert_eq!(
        actions,
        vec![Action::Reply(Err(
            TxProtocolError::TransactionConnectionBusy
        ))]
    );
    assert_eq!(harness.state(), TxState::InFlight);
}

/// Invariant 13: no data or frame-open command may start from `Poisoned`, but
/// a `ROLLBACK TO` of the recovery child returns the parent to `Idle`.
///
/// **This arc is the reason the forbidden-route arms exist.** It is a real,
/// shipped behaviour (`crates/zeroship-data-v8/src/tests/postgres/transactions.rs`), so it cannot be
/// removed to make forced cleanup safe; the force has to avoid `Poisoned`
/// instead.
///
/// Where this would fail today: on an implementation that makes `Poisoned`
/// terminal, which would forbid the nested-inner-reject case the product
/// ships.
#[test]
fn poisoned_refuses_data_sql_but_a_rollback_to_recovers_it() {
    let (harness, child) = Harness::at(TxState::Idle).with_open_child();
    let mut harness = harness;
    harness.apply(TxEvent::OperationRequested);
    let token = harness.last_token();
    harness.apply(TxEvent::OperationCompleted {
        token,
        errored: true,
    });
    assert_eq!(harness.state(), TxState::Poisoned);

    // No data SQL from Poisoned.
    let actions = harness.apply(TxEvent::OperationRequested);
    assert_eq!(
        actions,
        vec![Action::Reply(Err(TxProtocolError::TransactionNotReady))]
    );
    // No frame open from Poisoned either.
    let actions = harness.apply(TxEvent::OpenFrame);
    assert_eq!(
        actions,
        vec![Action::Reply(Err(TxProtocolError::TransactionNotReady))]
    );

    // But the recovery rollback-to is legal, and it walks back to Idle.
    let actions = harness.apply(TxEvent::CloseFrame {
        frame: child,
        close: FrameClose::RolledBackTo,
    });
    let token = match actions.first() {
        Some(Action::IssueRollbackTo { token, .. }) => *token,
        other => panic!("expected IssueRollbackTo, got {other:?}"),
    };
    harness.apply(TxEvent::CloseFrameCompleted {
        token,
        frame: child,
        close: FrameClose::RolledBackTo,
        ok: true,
    });
    // ROLLBACK TO must be followed by RELEASE of the same name; the frame is
    // not closed until that lands.
    let release = harness.last_token();
    assert!(
        harness
            .all()
            .iter()
            .any(|action| matches!(action, Action::IssueRelease { .. }))
    );
    harness.apply(TxEvent::CloseFrameCompleted {
        token: release,
        frame: child,
        close: FrameClose::Released,
        ok: true,
    });
    assert_eq!(
        harness.state(),
        TxState::Idle,
        "invariant 13: a successful rollback-to of the recovery child returns \
         the parent to Idle"
    );
}

// ---------------------------------------------------------------------------
// Publication
// ---------------------------------------------------------------------------

/// Invariant 12 and the L8 case: a `COMMIT` answered `ROLLBACK` is a failed
/// transaction and publishes nothing.
///
/// Where this would fail today: on an implementation that reads the result
/// code rather than the command tag - the exact defect L8 closed, in which the
/// settle path reported success and went on to publish change events for
/// writes that never landed.
#[test]
fn a_commit_postgres_rolled_back_publishes_nothing() {
    let mut harness = Harness::at(TxState::Idle);
    harness
        .reducer
        .queue_effect(Effect::new("notes", "never-landed"))
        .unwrap();
    harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Commit,
    });
    let token = harness.last_token();
    harness.apply(TxEvent::TerminalCompleted {
        token,
        result: TerminalResult::RolledBack,
    });

    assert_eq!(harness.reducer.outcome(), Some(TerminalOutcome::RolledBack));
    assert!(!TerminalOutcome::RolledBack.publishes());
    assert!(
        !harness
            .all()
            .iter()
            .any(|action| matches!(action, Action::PublishEffects(_))),
        "a transaction PostgreSQL rolled back must not be reported as a \
         successful commit, and must publish nothing"
    );
    assert!(harness.all().contains(&Action::DiscardEffects));
}

/// Invariant 12's intent half: a `Committed` result against a **rollback**
/// intent is a terminal-result mismatch, not a commit.
///
/// Where this would fail today: on an implementation that keys publication on
/// the result alone. That half is easy to omit, and omitting it means a
/// backend that answers the wrong terminal verb gets to publish.
#[test]
fn a_committed_result_against_a_rollback_intent_publishes_nothing() {
    let mut harness = Harness::at(TxState::Idle);
    harness
        .reducer
        .queue_effect(Effect::new("notes", "should-not-publish"))
        .unwrap();
    harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Rollback,
    });
    let token = harness.last_token();
    harness.apply(TxEvent::TerminalCompleted {
        token,
        result: TerminalResult::Committed,
    });
    assert_eq!(
        harness.reducer.outcome(),
        Some(TerminalOutcome::ResultMismatch)
    );
    assert!(
        !harness
            .all()
            .iter()
            .any(|action| matches!(action, Action::PublishEffects(_)))
    );
}

/// A confirmed root commit publishes every retained effect exactly once, in
/// order.
#[test]
fn a_confirmed_commit_publishes_every_retained_effect_in_order() {
    let mut harness = Harness::at(TxState::Idle);
    for payload in ["one", "two", "three"] {
        harness
            .reducer
            .queue_effect(Effect::new("notes", payload))
            .unwrap();
    }
    harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Commit,
    });
    let token = harness.last_token();
    harness.apply(TxEvent::TerminalCompleted {
        token,
        result: TerminalResult::Committed,
    });

    let published: Vec<Vec<Box<str>>> = harness
        .all()
        .iter()
        .filter_map(|action| match action {
            Action::PublishEffects(effects) => {
                Some(effects.iter().map(|e| e.payload.clone()).collect())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        published,
        vec![vec!["one".into(), "two".into(), "three".into()]],
        "exactly one publication, in order"
    );
}

/// A rolled-back child's effects are never published, even when the root
/// commits.
///
/// Where this would fail today: on a flat app-keyed effect buffer, where a
/// top-level `COMMIT` drains the rolled-back child's queue and tells a
/// subscriber about a row that does not exist.
#[test]
fn a_rolled_back_frames_effects_are_never_published_by_the_root_commit() {
    let (harness, child) = Harness::at(TxState::Idle).with_open_child();
    let mut harness = harness;
    harness
        .reducer
        .queue_effect(Effect::new("notes", "child-write"))
        .unwrap();

    let actions = harness.apply(TxEvent::CloseFrame {
        frame: child,
        close: FrameClose::RolledBackTo,
    });
    let token = match actions.first() {
        Some(Action::IssueRollbackTo { token, .. }) => *token,
        other => panic!("expected IssueRollbackTo, got {other:?}"),
    };
    harness.apply(TxEvent::CloseFrameCompleted {
        token,
        frame: child,
        close: FrameClose::RolledBackTo,
        ok: true,
    });
    let release = harness.last_token();
    harness.apply(TxEvent::CloseFrameCompleted {
        token: release,
        frame: child,
        close: FrameClose::Released,
        ok: true,
    });
    assert_eq!(harness.state(), TxState::Idle);

    harness
        .reducer
        .queue_effect(Effect::new("notes", "root-write"))
        .unwrap();
    harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Commit,
    });
    let token = harness.last_token();
    harness.apply(TxEvent::TerminalCompleted {
        token,
        result: TerminalResult::Committed,
    });

    let published: Vec<Box<str>> = harness
        .all()
        .iter()
        .filter_map(|action| match action {
            Action::PublishEffects(effects) => Some(effects.clone()),
            _ => None,
        })
        .flatten()
        .map(|effect| effect.payload)
        .collect();
    assert_eq!(
        published,
        vec!["root-write".into()],
        "the rolled-back child's write is not published"
    );
}

// ---------------------------------------------------------------------------
// The gate's middle outcome
// ---------------------------------------------------------------------------

/// Gate rule 2: once a terminal completion is promised, a later forcing
/// publisher neither sets a new force nor suppresses the promised result.
///
/// Where this would fail today: on an implementation whose deadline can claim
/// a transaction that already committed - turning a succeeded commit into a
/// cancellation microseconds after the fact.
#[test]
fn a_deadline_firing_after_a_commit_succeeded_does_not_cancel_it() {
    let mut harness = Harness::at(TxState::Idle);
    harness
        .reducer
        .queue_effect(Effect::new("notes", "durable"))
        .unwrap();
    harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Commit,
    });
    let token = harness.last_token();
    harness.apply(TxEvent::TerminalCompleted {
        token,
        result: TerminalResult::Committed,
    });
    assert_eq!(harness.reducer.outcome(), Some(TerminalOutcome::Committed));

    // A late force arrives.
    let actions = harness.apply(TxEvent::Cancel {
        authority: authority(),
    });
    assert!(
        actions.is_empty(),
        "a late cancel joins and changes nothing"
    );
    assert_eq!(
        harness.reducer.outcome(),
        Some(TerminalOutcome::Committed),
        "the promised result survives"
    );
    assert!(harness.reducer.cleanup().is_none(), "no cause was latched");
}

/// `Settling` is the one nonterminal state a force cannot claim.
///
/// Where this would fail today: on an implementation that lets a force claim
/// `Settling`, which starts a competing cleanup on a session that is already
/// ending.
#[test]
fn a_force_cannot_claim_settling() {
    let mut harness = Harness::at(TxState::Idle);
    harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Commit,
    });
    assert_eq!(harness.state(), TxState::Settling);

    let actions = harness.apply(TxEvent::Cancel {
        authority: authority(),
    });
    assert!(
        actions.is_empty(),
        "it is a late cancel: it joins the terminal waiters"
    );
    assert_eq!(harness.state(), TxState::Settling);
    assert!(harness.reducer.cleanup().is_none());
    assert!(
        !TxState::Settling.is_forceable(),
        "the TerminalSql deadline, not a force, bounds this wait"
    );
}

// ---------------------------------------------------------------------------
// Claim balance, over the whole state set
// ---------------------------------------------------------------------------

/// Invariant 3: every granted admission has exactly one release, on every path
/// that reaches `Settled`.
///
/// Drives a force from every forceable state and every acknowledgement, plus
/// both second-stage deadlines, and asserts exactly one `ReleaseAdmission` per
/// transaction. This is the arm that would have caught DBR-11: a claim that
/// outlives the isolate parks every later transaction for that app.
///
/// Where this would fail today: on an implementation that releases the claim
/// on some cleanup paths and not others - which is what the shipped code does,
/// by its own admission (`transaction/mod.rs`: "if this op is CANCELLED
/// between taking the claim and the `BEGIN` returning ... the claim leaks and
/// later transactions for this app park until the isolate is evicted").
#[test]
fn every_granted_admission_has_exactly_one_release() {
    let acks = [
        CleanupAck::NoOpenTransaction,
        CleanupAck::RolledBack,
        CleanupAck::Indeterminate,
    ];
    let mut checked = 0;
    for state in TxState::FORCEABLE {
        for ack in acks {
            let mut harness = Harness::at(state);
            harness.apply(TxEvent::Cancel {
                authority: authority(),
            });
            let token = harness.last_token();
            harness.apply(TxEvent::CancellationAcknowledged { token, ack });
            assert_eq!(harness.state(), TxState::Settled, "{state:?}/{ack:?}");
            let releases = harness
                .all()
                .iter()
                .filter(|action| matches!(action, Action::ReleaseAdmission))
                .count();
            assert_eq!(releases, 1, "{state:?}/{ack:?}: exactly one release");
            checked += 1;
        }

        // And the path where the backend never answers at all.
        let mut harness = Harness::at(state);
        harness.apply(TxEvent::Cancel {
            authority: authority(),
        });
        let generation = match harness.reducer.deadline().state() {
            DeadlineState::Armed { generation, .. } => generation,
            other => panic!("{state:?}: expected an armed cleanup deadline, got {other:?}"),
        };
        harness.apply(TxEvent::DeadlineFired {
            kind: DeadlineKind::CancellationSql,
            generation,
        });
        assert_eq!(harness.state(), TxState::Settled, "{state:?}: bounded");
        assert_eq!(
            harness
                .all()
                .iter()
                .filter(|action| matches!(action, Action::ReleaseAdmission))
                .count(),
            1,
            "{state:?}: the unanswered path releases exactly once"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        TxState::FORCEABLE.len() * (acks.len() + 1),
        "24 (state, disposition) pairs ruled on"
    );
}

/// Invariant 4: session ownership is never silently absent between confirmed
/// `BEGIN` and terminal cleanup.
///
/// Where this would fail today: on an implementation that drops the session
/// into a `None` slot while a command is out - which is rule 1's defect, where
/// an absent client is read as proof that terminal SQL ran.
#[test]
fn session_ownership_is_never_silently_absent_while_a_transaction_is_open() {
    let mut harness = Harness::at(TxState::Idle);
    assert_eq!(harness.reducer.session(), SessionOwnership::Registry);

    harness.apply(TxEvent::OperationRequested);
    let token = harness.last_token();
    assert_eq!(
        harness.reducer.session(),
        SessionOwnership::Command(token),
        "a command that owns the client is named, not absent"
    );

    harness.apply(TxEvent::OperationCompleted {
        token,
        errored: false,
    });
    assert_eq!(harness.reducer.session(), SessionOwnership::Registry);

    harness.apply(TxEvent::SettleRequested {
        intent: SettleIntent::Commit,
    });
    let terminal = harness.last_token();
    assert_eq!(
        harness.reducer.session(),
        SessionOwnership::Command(terminal)
    );
    harness.apply(TxEvent::TerminalCompleted {
        token: terminal,
        result: TerminalResult::Committed,
    });
    assert_eq!(harness.reducer.session(), SessionOwnership::None);
}

/// Invariant 8, through the reducer rather than on `meet` alone: a
/// mid-transaction raise changes nothing.
///
/// Where this would fail today: on an implementation that assigns the newly
/// read ceiling rather than meeting with it.
#[test]
fn a_mid_transaction_ceiling_raise_changes_nothing() {
    let mut harness = Harness::at(TxState::Idle);
    let begin = harness
        .reducer
        .begin_ceiling()
        .cloned()
        .expect("BEGIN captured a ceiling");
    assert!(begin.permits("support") && begin.permits("auto"));

    // An operation's pre-SQL read reports a BROADER ceiling.
    harness.apply(TxEvent::AuthorityObserved {
        authority: authority(),
        observed: Box::new(ObservedAuthority {
            ceiling: MaskCeiling::of(["support", "auto", "operator"]),
            ..*current_observation()
        }),
    });
    let effective = harness
        .reducer
        .effective_ceiling()
        .expect("the fold produced a value");
    assert!(
        !effective.permits("operator"),
        "a raise is ignored until a new top-level transaction"
    );
    assert!(effective.is_no_broader_than(&begin));

    // A LOWER value tightens the next authorization.
    harness.apply(TxEvent::AuthorityObserved {
        authority: authority(),
        observed: Box::new(ObservedAuthority {
            ceiling: MaskCeiling::of(["support"]),
            ..*current_observation()
        }),
    });
    let effective = harness.reducer.effective_ceiling().unwrap();
    assert!(effective.permits("support"));
    assert!(!effective.permits("auto"), "a lower value tightens");
}

/// A failed `BEGIN` reaches `Cancelling` without a publisher to arbitrate
/// against, and issues no data SQL.
///
/// Where this would fail today: on an implementation that leaves a failed
/// `BEGIN` in `Starting` or drops it into `Poisoned`, both of which strand the
/// admission claim - `Starting` because nothing else ends it, `Poisoned`
/// because a creator command can walk it back to `Idle`.
#[test]
fn a_failed_begin_enters_cancelling_with_the_abort_if_opened_goal() {
    let mut harness = Harness::at(TxState::Starting);
    let token = harness.last_token();
    harness.apply(TxEvent::BeginCompleted {
        token,
        outcome: BeginOutcome::Failed,
    });
    assert_eq!(harness.state(), TxState::Cancelling);
    assert_eq!(
        harness.reducer.cleanup(),
        Some(LatchedCleanup {
            cause: CleanupCause::BeginFailed,
            goal: CleanupGoal::AbortIfOpened,
        }),
        "a BEGIN that may or may not have opened cannot demand OpenTransaction"
    );
}

/// A creator-facing setup classification remains distinct from an
/// unclassified BEGIN failure, so the driver can return its exact `DbError`
/// without naming each code in the transaction orchestrator.
#[test]
fn a_classified_setup_failure_does_not_collapse_into_begin_failed() {
    let mut harness = Harness::at(TxState::Starting);
    let token = harness.last_token();
    harness.apply(TxEvent::BeginCompleted {
        token,
        outcome: BeginOutcome::SetupFailed,
    });
    assert_eq!(harness.state(), TxState::Cancelling);
    assert_eq!(
        harness.reducer.cleanup(),
        Some(LatchedCleanup {
            cause: CleanupCause::SessionSetupFailed,
            goal: CleanupGoal::AbortIfOpened,
        })
    );
    assert_ne!(
        harness.reducer.cleanup().map(|cleanup| cleanup.cause),
        Some(CleanupCause::BeginFailed)
    );
}

/// A role-name epoch fence can report `ReResolve` through the same begin event
/// without another event-shape change. This tests the seam only; there is no
/// schema-epoch producer in this task.
#[test]
fn a_begin_setup_re_resolve_uses_the_existing_retryable_verdict_arm() {
    let mut harness = Harness::at(TxState::Starting);
    let token = harness.last_token();
    let actions = harness.apply(TxEvent::BeginCompleted {
        token,
        outcome: BeginOutcome::ReResolve,
    });
    assert_eq!(harness.state(), TxState::Cancelling);
    assert_eq!(
        harness.reducer.cleanup().map(|cleanup| cleanup.cause),
        Some(CleanupCause::EpochChanged)
    );
    assert!(CleanupCause::EpochChanged.retryable());
    assert!(actions.contains(&Action::Reply(Err(TxProtocolError::EpochChanged))));
}

/// A revoked role membership is terminal and keeps its specific denial rather
/// than taking either the retryable re-resolution route or generic BEGIN.
#[test]
fn a_begin_setup_grant_revoke_is_a_specific_terminal_denial() {
    let mut harness = Harness::at(TxState::Starting);
    let token = harness.last_token();
    let actions = harness.apply(TxEvent::BeginCompleted {
        token,
        outcome: BeginOutcome::Denied(DenyReason::GrantRevoked),
    });
    assert_eq!(harness.state(), TxState::Cancelling);
    assert_eq!(
        harness.reducer.cleanup().map(|cleanup| cleanup.cause),
        Some(CleanupCause::Denied(DenyReason::GrantRevoked))
    );
    assert!(!DenyReason::GrantRevoked.retryable());
    assert!(actions.contains(&Action::Reply(Err(TxProtocolError::Denied(
        DenyReason::GrantRevoked
    )))));
}

/// The budget is armed on entry to `Preparing`, so queue time does not consume
/// it.
///
/// Where this would fail today: on an implementation that arms the deadline
/// before admission - which charges a transaction for time it spent waiting
/// for a slot another transaction held.
#[test]
fn the_execution_deadline_is_armed_on_admission_not_before_it() {
    let now = Instant::now();
    let budgets = TxBudgets {
        execution: Duration::from_secs(7),
        ..TxBudgets::default()
    };
    let (reducer, actions) = TxReducer::admit(expected(), budgets, now, MAX_DEPTH);
    assert_eq!(reducer.state(), TxState::Preparing);
    match actions.as_slice() {
        [Action::ScheduleTimer(scheduled)] => {
            assert_eq!(scheduled.kind, DeadlineKind::Execution);
            assert_eq!(
                scheduled.at,
                now + Duration::from_secs(7),
                "the budget starts at admission, not at queue entry"
            );
        }
        other => panic!("expected exactly one ScheduleTimer, got {other:?}"),
    }
}
