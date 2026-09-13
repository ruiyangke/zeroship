use super::*;
use futures::{channel::oneshot, FutureExt};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};

#[test]
fn watchdog_interrupts_even_when_the_executor_thread_is_blocked() {
    let guard = ExecutionGuard::new(Duration::from_millis(30)).unwrap();
    let budget = guard.budget();
    let (sender, receiver) = mpsc::channel();
    let caller = std::thread::current().id();
    budget
        .on_interrupt(move || {
            sender.send(std::thread::current().id()).unwrap();
        })
        .unwrap();
    let watchdog = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_ne!(watchdog, caller);
    assert!(matches!(budget.check(), Err(WorkflowServiceError::Timeout)));
    drop(guard);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn cancellation_before_loading_cannot_lose_a_late_interrupt() {
    let guard = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    let budget = guard.budget();
    drop(guard);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    budget
        .on_interrupt(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(budget.check(), Err(WorkflowServiceError::Timeout)));
    assert!(budget
        .on_interrupt(|| panic!("replacement must be refused"))
        .is_err());
    drop(budget);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn renewal_cannot_revive_expired_authority() {
    let guard = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    let budget = guard.budget();
    let (sender, receiver) = mpsc::channel();
    budget
        .on_interrupt(move || {
            sender.send(()).unwrap();
        })
        .unwrap();
    assert!(matches!(
        guard.renew_lease(Instant::now()),
        Err(WorkflowServiceError::Timeout)
    ));
    receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        guard.renew_lease(Instant::now() + Duration::from_secs(30)),
        Err(WorkflowServiceError::Timeout)
    ));
    assert!(matches!(budget.check(), Err(WorkflowServiceError::Timeout)));
    assert!(receiver.try_recv().is_err());
}

#[test]
fn renewing_a_lease_does_not_extend_the_execution_deadline() {
    let guard = ExecutionGuard::new(Duration::from_millis(60)).unwrap();
    let (sender, receiver) = mpsc::channel();
    guard
        .budget()
        .on_interrupt(move || {
            sender.send(()).unwrap();
        })
        .unwrap();
    guard
        .renew_lease(Instant::now() + Duration::from_secs(30))
        .unwrap();
    receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        guard.budget().check(),
        Err(WorkflowServiceError::Timeout)
    ));
}

#[test]
fn finished_execution_releases_its_interrupt_and_allows_completion_renewal() {
    let guard = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    let budget = guard.budget();
    let weak = Arc::downgrade(&budget.0);
    let captured = budget.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    budget
        .on_interrupt(move || {
            // Interrupts run outside the registry lock and may inspect their budget.
            assert!(matches!(
                captured.check(),
                Err(WorkflowServiceError::Timeout)
            ));
            observed.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    guard.finish();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    guard
        .renew_lease(Instant::now() + Duration::from_secs(30))
        .unwrap();
    assert!(matches!(budget.check(), Err(WorkflowServiceError::Timeout)));
    drop(guard);
    drop(budget);
    assert!(
        weak.upgrade().is_none(),
        "callback capture must not retain its registration"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_of_one_budget_preserves_another() {
    let cancelled = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    let live = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    drop(cancelled);
    live.budget().check().unwrap();
}

#[test]
fn policy_cancellation_interrupts_while_the_executor_thread_is_blocked() {
    let guard = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    let live = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    let budget = guard.budget();
    let (cancel, mut cancellation) = oneshot::channel::<()>();
    let (subscribed, subscription) = mpsc::channel();
    let mut subscribed = Some(subscribed);
    let check = budget.clone();
    guard
        .cancel_on(
            futures::future::poll_fn(move |cx| {
                // Polling a host signal must not hold the budget registry lock.
                check.check().unwrap();
                let result = cancellation.poll_unpin(cx).map(|_| ());
                if let Some(subscribed) = subscribed.take() {
                    subscribed.send(()).unwrap();
                }
                result
            })
            .boxed()
            .shared(),
        )
        .unwrap();
    let (interrupted, interruption) = mpsc::channel();
    let check = budget.clone();
    budget
        .on_interrupt(move || {
            // The interrupt is also delivered without the registry lock.
            assert!(check.check().is_err());
            interrupted.send(std::thread::current().id()).unwrap();
        })
        .unwrap();
    subscription.recv_timeout(Duration::from_secs(5)).unwrap();
    cancel.send(()).unwrap();
    let interrupted_on = interruption.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_ne!(interrupted_on, std::thread::current().id());
    assert!(matches!(budget.check(), Err(WorkflowServiceError::Timeout)));
    assert!(matches!(
        guard.renew_lease(Instant::now() + Duration::from_secs(30)),
        Err(WorkflowServiceError::Timeout)
    ));
    assert!(guard
        .cancel_on(futures::future::pending().boxed().shared())
        .is_err());
    live.budget().check().unwrap();
    guard.finish();
    assert!(interruption.try_recv().is_err());
}

#[test]
fn ready_policy_cancellation_cannot_lose_a_late_interrupt() {
    let guard = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    let budget = guard.budget();
    guard
        .cancel_on(futures::future::ready(()).boxed().shared())
        .unwrap();
    let (interrupted, interruption) = mpsc::channel();
    budget
        .on_interrupt(move || interrupted.send(()).unwrap())
        .unwrap();
    interruption.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(budget.check(), Err(WorkflowServiceError::Timeout)));
    assert!(matches!(
        guard.renew_lease(Instant::now() + Duration::from_secs(30)),
        Err(WorkflowServiceError::Timeout)
    ));
    drop(guard);
    assert!(interruption.try_recv().is_err());
}

#[test]
fn a_policy_wake_during_poll_is_not_lost_before_watchdog_sleep() {
    let guard = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let observed = polls.clone();
    guard
        .cancel_on(
            futures::future::poll_fn(move |cx| {
                if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(())
                }
            })
            .boxed()
            .shared(),
        )
        .unwrap();
    let (interrupted, interruption) = mpsc::channel();
    guard
        .budget()
        .on_interrupt(move || interrupted.send(()).unwrap())
        .unwrap();
    interruption.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert!(guard.budget().check().is_err());
}

struct SignalLifetime {
    budget: Option<ExecutionBudget>,
    dropped: mpsc::Sender<()>,
}
impl Drop for SignalLifetime {
    fn drop(&mut self) {
        assert!(self.budget.as_ref().unwrap().check().is_err());
        drop(self.budget.take());
        self.dropped.send(()).unwrap();
    }
}

#[test]
fn finished_execution_drops_pending_policy_signal_without_retaining_registration() {
    let guard = ExecutionGuard::new(Duration::from_secs(30)).unwrap();
    let budget = guard.budget();
    let registration = Arc::downgrade(&budget.0);
    let (dropped, drop_observed) = mpsc::channel();
    let lifetime = SignalLifetime {
        budget: Some(budget.clone()),
        dropped,
    };
    guard
        .cancel_on(
            async move {
                let _lifetime = lifetime;
                futures::future::pending::<()>().await;
            }
            .boxed()
            .shared(),
        )
        .unwrap();
    guard.finish();
    drop_observed.recv_timeout(Duration::from_secs(5)).unwrap();
    guard
        .renew_lease(Instant::now() + Duration::from_secs(30))
        .unwrap();
    drop(guard);
    drop(budget);
    assert!(registration.upgrade().is_none());
}
