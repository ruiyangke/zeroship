use super::*;
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
