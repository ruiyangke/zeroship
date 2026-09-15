use super::*;
use crate::driver::{CancellationHandle, DriverSession};
use crate::transaction::reducer::CleanupCause;
use crate::value::Value;
use futures::{channel::oneshot, FutureExt};
use std::{cell::Cell, cell::RefCell, future::Future, rc::Rc, task::Poll};

type TerminalAnswer = (TerminalResult, Option<DbError>);

#[derive(Debug)]
struct ControlledSession {
    wire: Rc<Wire>,
    terminal: RefCell<Option<oneshot::Receiver<TerminalAnswer>>>,
    cleanup: RefCell<Option<oneshot::Receiver<CleanupAck>>>,
}

#[derive(Default, Debug)]
struct Wire {
    terminal_calls: Cell<usize>,
    cleanup_calls: Cell<usize>,
    discarded: Cell<bool>,
}

#[async_trait::async_trait(?Send)]
impl DriverSession for ControlledSession {
    async fn query(&self, _: &str, _: &[Value]) -> Result<Vec<Value>, DbError> {
        panic!("this fixture only accepts terminal commands")
    }
    async fn exec(&self, _: &str, _: &[Value]) -> Result<u64, DbError> {
        panic!("this fixture only accepts terminal commands")
    }
    async fn settle(&self, _: SettleIntent) -> TerminalAnswer {
        self.wire
            .terminal_calls
            .set(self.wire.terminal_calls.get() + 1);
        let receiver = self
            .terminal
            .borrow_mut()
            .take()
            .expect("terminal issued once");
        receiver.await.expect("test supplies terminal answer")
    }
    async fn cleanup(&self) -> CleanupAck {
        self.wire
            .cleanup_calls
            .set(self.wire.cleanup_calls.get() + 1);
        let receiver = self
            .cleanup
            .borrow_mut()
            .take()
            .expect("cleanup issued once");
        receiver.await.expect("test supplies cleanup answer")
    }
    fn canceller(&self) -> Option<CancellationHandle> {
        None
    }
    fn discard(self: Box<Self>) {
        self.wire.discarded.set(true);
    }
}

struct Fixture {
    wire: Rc<Wire>,
    terminal: oneshot::Sender<TerminalAnswer>,
    cleanup: oneshot::Sender<CleanupAck>,
    generation: BackendGeneration,
}

fn admitted(app: &str) -> Fixture {
    crate::tx_lanes::with_mut(|l| assert!(l.try_claim_tx(app)));
    admitted_on_claim(app)
}

fn admitted_on_claim(app: &str) -> Fixture {
    admit_in_preparing(app);
    let expected = expected_authority(app);
    let actions = apply(
        app,
        TxEvent::AuthorityObserved {
            authority: authority_of(app).unwrap(),
            observed: Box::new(observation_for(&expected)),
        },
    )
    .unwrap();
    let token = actions
        .into_iter()
        .find_map(|a| match a {
            Action::IssueBegin { token } => Some(token),
            _ => None,
        })
        .unwrap();
    let generation = BackendGeneration(next_backend_generation());
    apply(
        app,
        TxEvent::BeginCompleted {
            token,
            outcome: BeginOutcome::Opened(generation),
        },
    )
    .unwrap();
    let wire = Rc::new(Wire::default());
    let (terminal, terminal_rx) = oneshot::channel();
    let (cleanup, cleanup_rx) = oneshot::channel();
    install(
        app,
        Session::new(ControlledSession {
            wire: wire.clone(),
            terminal: RefCell::new(Some(terminal_rx)),
            cleanup: RefCell::new(Some(cleanup_rx)),
        }),
    );
    Fixture {
        wire,
        terminal,
        cleanup,
        generation,
    }
}

fn in_flight(app: &str) -> CommandToken {
    apply(app, TxEvent::OperationRequested)
        .unwrap()
        .into_iter()
        .find_map(|a| match a {
            Action::IssueDataSql { token } => Some(token),
            _ => None,
        })
        .unwrap()
}

fn assert_pending<F: Future>(future: std::pin::Pin<&mut F>) {
    assert!(matches!(
        future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop())),
        Poll::Pending
    ));
}

fn deadline(app: &str) -> (DeadlineKind, DeadlineGeneration) {
    crate::tx_lanes::with(
        |l| match l.transaction_reducer(app).unwrap().deadline().state() {
            super::super::reducer::deadline::DeadlineState::Armed {
                kind, generation, ..
            } => (kind, generation),
            other => panic!("expected armed deadline, got {other:?}"),
        },
    )
}

#[compio::test]
async fn settlement_waits_for_operation_and_terminal_and_survives_lane_replacement() {
    for intent in [SettleIntent::Commit, SettleIntent::Rollback] {
        let owner = crate::OrmContext::new();
        owner
            .scope(async {
                let app = "settlement_wait";
                let fixture = admitted(app);
                let token = in_flight(app);
                let mut first = Box::pin(settle_root(app, intent));
                let mut second = Box::pin(settle_root(app, intent));
                assert_pending(first.as_mut());
                assert_pending(second.as_mut());
                assert_eq!(fixture.wire.terminal_calls.get(), 0);
                let opposite = if intent == SettleIntent::Commit {
                    SettleIntent::Rollback
                } else {
                    SettleIntent::Commit
                };
                assert_eq!(
                    settle_root(app, opposite).await.refusal(),
                    Some(TxProtocolError::SettleConflict)
                );

                let mut operation =
                    Box::pin(complete_operation(app, fixture.generation, token, false));
                assert_pending(operation.as_mut());
                assert_eq!(fixture.wire.terminal_calls.get(), 1);
                assert_pending(first.as_mut());
                assert_pending(second.as_mut());
                assert_eq!(
                    settle_root(app, opposite).await.refusal(),
                    Some(TxProtocolError::SettleConflict)
                );
                let answer = if intent == SettleIntent::Commit {
                    TerminalResult::Committed
                } else {
                    TerminalResult::RolledBack
                };
                fixture.terminal.send((answer, None)).unwrap();
                let expected = operation.await.outcome().unwrap();
                assert!(!crate::tx_lanes::with(|l| l.tx_claimed_by(app)));

                let replacement = admitted(app);
                assert_eq!(first.await.outcome(), Some(expected));
                assert_eq!(second.await.outcome(), Some(expected));
                assert_eq!(operation_generation(app), Some(replacement.generation));
                assert_eq!(replacement.wire.terminal_calls.get(), 0);
                crate::tx_lanes::with_mut(|l| l.release_tx_claim(app));
            })
            .await;
    }
}

#[compio::test]
async fn deferred_terminal_failure_reaches_every_settlement_waiter() {
    crate::OrmContext::new()
        .scope(async {
            let app = "failed_settlement";
            let fixture = admitted(app);
            let token = in_flight(app);
            let mut settle = Box::pin(settle_root(app, SettleIntent::Rollback));
            assert_pending(settle.as_mut());
            let mut operation = Box::pin(complete_operation(app, fixture.generation, token, false));
            assert_pending(operation.as_mut());
            fixture
                .terminal
                .send((
                    TerminalResult::Indeterminate,
                    Some(DbError::internal("terminal transport failed")),
                ))
                .unwrap();
            operation.await;
            let result = settle.await;
            assert!(matches!(
                result.outcome(),
                Some(TerminalOutcome::Indeterminate(_))
            ));
            assert_eq!(
                result.error.unwrap().message_str(),
                "terminal transport failed"
            );
            assert!(fixture.wire.discarded.get());
        })
        .await;
}

#[compio::test]
async fn terminal_deadline_wakes_waiter_and_late_answer_cannot_restore_old_session() {
    crate::OrmContext::new()
        .scope(async {
            let app = "deadline_settlement";
            let fixture = admitted(app);
            let token = in_flight(app);
            let mut settle = Box::pin(settle_root(app, SettleIntent::Commit));
            assert_pending(settle.as_mut());
            let mut operation = Box::pin(complete_operation(app, fixture.generation, token, false));
            assert_pending(operation.as_mut());
            let (kind, generation) = deadline(app);
            let terminal = deadline_fired(app, kind, generation).await.outcome();
            assert!(matches!(terminal, Some(TerminalOutcome::Indeterminate(_))));
            assert_eq!(settle.await.outcome(), terminal);
            let replacement = admitted(app);
            let replacement_holder = crate::tx_lanes::TxClientSlotGuard::take(app).unwrap();
            fixture
                .terminal
                .send((TerminalResult::Committed, None))
                .unwrap();
            assert_eq!(operation.await.outcome(), terminal);
            assert!(fixture.wire.discarded.get());
            assert!(
                !crate::tx_lanes::with(|l| l.has_tx_for(app)),
                "old client must not refill a replacement's temporarily empty slot"
            );
            assert_eq!(operation_generation(app), Some(replacement.generation));
            drop(replacement_holder);
            crate::tx_lanes::with_mut(|l| l.release_tx_claim(app));
        })
        .await;
}

#[compio::test]
async fn cancellation_and_execution_deadline_publish_only_after_cleanup() {
    for forced_by_deadline in [false, true] {
        crate::OrmContext::new()
            .scope(async {
                let app = "cancel_settlement";
                let fixture = admitted(app);
                in_flight(app);
                let mut settle = Box::pin(settle_root(app, SettleIntent::Rollback));
                assert_pending(settle.as_mut());
                let mut force = Box::pin(async {
                    if forced_by_deadline {
                        let (kind, generation) = deadline(app);
                        deadline_fired(app, kind, generation).await
                    } else {
                        cancel(app).await
                    }
                });
                assert_pending(force.as_mut());
                assert_eq!(fixture.wire.cleanup_calls.get(), 1);
                assert_pending(settle.as_mut());
                let mut joining = Box::pin(settle_root(app, SettleIntent::Rollback));
                assert_pending(joining.as_mut());
                fixture.cleanup.send(CleanupAck::RolledBack).unwrap();
                let terminal = force.await.outcome();
                assert!(matches!(terminal, Some(TerminalOutcome::Cancelled(_))));
                assert_eq!(settle.await.outcome(), terminal);
                assert_eq!(joining.await.outcome(), terminal);
                assert_eq!(fixture.wire.terminal_calls.get(), 0);
            })
            .await;
    }
}

#[compio::test]
async fn dropping_waiter_does_not_cancel_accepted_settlement() {
    crate::OrmContext::new()
        .scope(async {
            let app = "dropped_settlement";
            let fixture = admitted(app);
            let mut first = Box::pin(settle_root(app, SettleIntent::Rollback));
            assert_pending(first.as_mut());
            drop(first);
            fixture
                .terminal
                .send((TerminalResult::RolledBack, None))
                .unwrap();
            assert_eq!(
                settle_root(app, SettleIntent::Rollback).await.outcome(),
                Some(TerminalOutcome::RolledBack)
            );
            assert_eq!(fixture.wire.terminal_calls.get(), 1);
            assert!(!fixture.wire.discarded.get());
        })
        .await;
}

#[compio::test]
async fn dropped_native_admission_stays_owned_until_cleanup_acknowledges_rollback() {
    crate::OrmContext::new()
        .scope(async {
            let app = "dropped_native_admission";
            let admission = super::super::TxAdmission::acquire(app.into())
                .await
                .expect("the fixture claims a free lane");
            let fixture = admitted_on_claim(app);
            drop(admission);
            assert_eq!(
                crate::tx_lanes::with(|l| l.transaction_reducer(app).map(TxReducer::state)),
                Some(super::super::reducer::TxState::Cancelling)
            );
            let mut next = Box::pin(super::super::TxAdmission::acquire(app.into()));
            assert_pending(next.as_mut());
            compio::time::sleep(Duration::from_millis(1)).await;
            assert_eq!(fixture.wire.cleanup_calls.get(), 1);
            assert!(cancel(app).await.outcome().is_none());
            assert_eq!(fixture.wire.cleanup_calls.get(), 1);
            assert_pending(next.as_mut());
            fixture.cleanup.send(CleanupAck::RolledBack).unwrap();
            let next = compio::time::timeout(Duration::from_secs(1), next)
                .await
                .expect("a settled cleanup must release admission")
                .expect("a released lane is claimed, not refused");
            assert!(!fixture.wire.discarded.get());
            drop(next);
        })
        .await;
}

#[compio::test]
async fn dropped_retired_native_admission_cannot_cancel_a_replacement() {
    crate::OrmContext::new()
        .scope(async {
            let app = "retired_native_admission";
            let old_admission = super::super::TxAdmission::acquire(app.into())
                .await
                .expect("the fixture claims a free lane");
            let old = admitted_on_claim(app);
            old.cleanup.send(CleanupAck::RolledBack).unwrap();
            assert!(matches!(
                cancel(app).await.outcome(),
                Some(TerminalOutcome::Cancelled(_))
            ));
            let replacement = admitted(app);
            drop(old_admission);
            assert_eq!(
                crate::tx_lanes::with(|l| l.transaction_reducer(app).map(TxReducer::state)),
                Some(super::super::reducer::TxState::Idle)
            );
            assert!(!replacement.wire.discarded.get());
            replacement
                .terminal
                .send((TerminalResult::Committed, None))
                .unwrap();
            assert_eq!(
                settle_root(app, SettleIntent::Commit).await.outcome(),
                Some(TerminalOutcome::Committed)
            );
        })
        .await;
}

#[compio::test]
async fn startup_cancellation_without_an_installed_session_is_indeterminate() {
    crate::OrmContext::new()
        .scope(async {
            let app = "cancel_native_startup";
            let admission = super::super::TxAdmission::acquire(app.into())
                .await
                .expect("the fixture claims a free lane");
            admit_in_preparing(app);
            let opening = apply(
                app,
                TxEvent::AuthorityObserved {
                    authority: authority_of(app).unwrap(),
                    observed: Box::new(observation_for(&expected_authority(app))),
                },
            )
            .unwrap();
            let token = opening
                .into_iter()
                .find_map(|action| match action {
                    Action::IssueBegin { token } => Some(token),
                    _ => None,
                })
                .unwrap();
            let completion = crate::tx_lanes::with(|l| l.transaction_completion(app)).unwrap();
            drop(admission);
            assert_eq!(
                completion.wait().await.outcome(),
                Some(TerminalOutcome::Indeterminate(CleanupCause::Cancelled))
            );
            let replacement = admitted(app);
            let late_wire = Rc::new(Wire::default());
            let late_session = Session::new(ControlledSession {
                wire: late_wire.clone(),
                terminal: RefCell::new(None),
                cleanup: RefCell::new(None),
            });
            assert!(install_opened_session(app, &completion, late_session).is_err());
            assert!(late_wire.discarded.get());
            assert!(!replacement.wire.discarded.get());
            let mut late = VecDeque::new();
            extend(
                &mut late,
                app,
                &completion,
                TxEvent::BeginCompleted {
                    token,
                    outcome: BeginOutcome::Opened(BackendGeneration(next_backend_generation())),
                },
            );
            assert!(late.is_empty());
            assert_eq!(operation_generation(app), Some(replacement.generation));
            replacement
                .terminal
                .send((TerminalResult::Committed, None))
                .unwrap();
            assert_eq!(
                settle_root(app, SettleIntent::Commit).await.outcome(),
                Some(TerminalOutcome::Committed)
            );
        })
        .await;
}

#[compio::test]
async fn retiring_lane_resolves_waiter_without_claiming_success() {
    crate::OrmContext::new()
        .scope(async {
            let app = "retired_settlement";
            let _fixture = admitted(app);
            in_flight(app);
            let mut settle = Box::pin(settle_root(app, SettleIntent::Rollback));
            assert_pending(settle.as_mut());
            crate::tx_lanes::with_mut(|l| l.retire_transaction(app));
            let result = settle
                .now_or_never()
                .expect("retirement must wake the waiter");
            assert_eq!(result.refusal(), Some(TxProtocolError::TransactionNotReady));
            assert!(result.outcome().is_none());
            crate::tx_lanes::with_mut(|l| l.release_tx_claim(app));
        })
        .await;
}
