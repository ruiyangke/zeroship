use super::*;
use futures::{future::pending, task::ArcWake};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
    time::Duration,
};

fn revision(value: i64) -> Revision {
    value.try_into().unwrap()
}

fn configured(value: i64) -> PolicySnapshot {
    PolicySnapshot::configuration(revision(value), AppPolicy::default()).unwrap()
}

fn leased(value: i64, until: Instant) -> PolicySnapshot {
    PolicySnapshot::lease(revision(value), AppPolicy::default(), until).unwrap()
}

fn registry() -> (Arc<HostPolicies>, PolicyBinding) {
    let policies = Arc::new(HostPolicies::default());
    let binding = policies.bind(AppId::mint()).unwrap();
    (policies, binding)
}

fn install(binding: &PolicyBinding, snapshot: PolicySnapshot) {
    binding.begin_refresh().unwrap().install(snapshot).unwrap();
}

fn assert_unavailable<T>(result: Result<T, WorkflowServiceError>) {
    assert!(matches!(
        result.err(),
        Some(WorkflowServiceError::Unavailable(_))
    ));
}

fn assert_conflict<T>(result: Result<T, WorkflowServiceError>) {
    assert!(matches!(
        result.err(),
        Some(WorkflowServiceError::Conflict(_))
    ));
}

#[compio::test]
async fn bound_host_setup_rejects_foreign_registry_before_polling() {
    let (policies, binding) = registry();
    install(&binding, configured(1));
    let foreign = Arc::new(HostPolicies::default());
    let foreign_binding = foreign.bind(binding.app_id().clone()).unwrap();
    install(&foreign_binding, configured(1));
    let polled = std::cell::Cell::new(false);
    let result = policies
        .run_bound(&foreign_binding, async {
            polled.set(true);
            Ok(())
        })
        .await;
    assert!(matches!(
        result,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(!polled.get());
    policies
        .run_bound(&binding, async { Ok(()) })
        .await
        .unwrap();
}

#[compio::test]
async fn bound_host_setup_captures_authority_before_first_poll() {
    let (policies, binding) = registry();
    let polled = std::cell::Cell::new(false);
    let setup = policies.run_bound(&binding, async {
        polled.set(true);
        Ok(())
    });
    install(&binding, configured(1));
    assert_unavailable(setup.await);
    assert!(!polled.get());

    let binding = policies.bind(binding.app_id().clone()).unwrap();
    let deadline = Instant::now() + Duration::from_millis(20);
    install(&binding, leased(1, deadline));
    let setup = policies.run_bound(&binding, pending::<Result<(), WorkflowServiceError>>());
    install(&binding, leased(1, Instant::now() + Duration::from_secs(5)));
    assert_unavailable(
        compio::time::timeout(Duration::from_secs(1), setup)
            .await
            .unwrap(),
    );
    binding.authority().unwrap().check().unwrap();
}

#[compio::test]
async fn bound_host_setup_drops_native_work_when_generation_is_replaced() {
    struct Stopped<'a>(&'a std::cell::Cell<bool>);
    impl Drop for Stopped<'_> {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    let (policies, binding) = registry();
    install(&binding, configured(1));
    let stopped = std::cell::Cell::new(false);
    let mut setup = policies.run_bound(&binding, async {
        let _stopped = Stopped(&stopped);
        pending::<Result<(), WorkflowServiceError>>().await
    });
    assert!(futures::poll!(setup.as_mut()).is_pending());
    let replacement = policies.bind(binding.app_id().clone()).unwrap();
    install(&replacement, configured(1));
    assert_unavailable(setup.await);
    assert!(stopped.get());
    policies
        .run_bound(&replacement, async { Ok(()) })
        .await
        .unwrap();
}

#[test]
fn replacement_retires_capabilities_and_outstanding_refreshes() {
    let (policies, original) = registry();
    assert!(policies.app_ids().unwrap().is_empty());
    assert_unavailable(original.resolve());
    install(&original, configured(3));
    let capture = original.authority().unwrap();
    let delayed = original.begin_refresh().unwrap();
    let replacement = policies.bind(original.app_id().clone()).unwrap();
    assert!(replacement.belongs_to(&policies));
    assert!(!original.same_binding(&replacement));
    assert!(original.same_binding(&original.clone()));
    assert!(capture.belongs_to(&original));
    assert!(!capture.belongs_to(&replacement));
    assert_unavailable(capture.check());
    assert_unavailable(original.resolve());
    assert_unavailable(original.authority());
    assert_unavailable(original.begin_refresh());
    assert_unavailable(original.revoke());
    assert_unavailable(delayed.install(configured(4)));
    assert_unavailable(replacement.authority());
    install(&replacement, configured(3));
    replacement.authority().unwrap().check().unwrap();
    assert!(policies
        .current_binding(original.app_id())
        .unwrap()
        .same_binding(&replacement));
    let foreign_registry = Arc::new(HostPolicies::default());
    let foreign = foreign_registry.bind(original.app_id().clone()).unwrap();
    assert!(!replacement.same_binding(&foreign));
    assert!(!replacement.belongs_to(&foreign_registry));
    let other_app = policies.bind(AppId::mint()).unwrap();
    assert!(!replacement.same_binding(&other_app));
    assert_eq!(policies.app_ids().unwrap(), [original.app_id().clone()]);
}

#[test]
fn source_high_water_survives_revoke_and_replacement() {
    let (policies, original) = registry();
    install(&original, configured(4));
    let delayed = original.begin_refresh().unwrap();
    original.revoke().unwrap();
    original.revoke().unwrap();
    assert!(policies.app_ids().unwrap().is_empty());
    assert_unavailable(policies.current_binding(original.app_id()));
    assert_unavailable(delayed.install(configured(5)));
    let replacement = policies.bind(original.app_id().clone()).unwrap();
    assert_conflict(replacement.begin_refresh().unwrap().install(configured(3)));
    let mut altered = AppPolicy::default();
    altered.max_running += 1;
    let altered = PolicySnapshot::configuration(revision(4), altered).unwrap();
    assert_conflict(replacement.begin_refresh().unwrap().install(altered));
    install(&replacement, configured(4));
    assert_unavailable(original.revoke());
    let captured = replacement.authority().unwrap();
    install(&replacement, configured(5));
    assert_unavailable(captured.check());
    assert_conflict(replacement.begin_refresh().unwrap().install(configured(4)));
    replacement.authority().unwrap().check().unwrap();
}

#[test]
fn refresh_ticket_is_ordered_and_consumed() {
    let (_, binding) = registry();
    install(&binding, configured(1));
    let captured = binding.authority().unwrap();
    let stale = binding.begin_refresh().unwrap();
    let current = binding.begin_refresh().unwrap();
    assert_unavailable(stale.install(configured(3)));
    captured.check().unwrap();
    // An internal duplicate verifies the state fence in addition to the public
    // consuming install API, which does not expose a cloneable ticket.
    let duplicate = PolicyRefresh {
        binding: binding.clone(),
        ticket: current.ticket,
    };
    current.install(configured(2)).unwrap();
    assert_unavailable(duplicate.install(configured(3)));
    assert_unavailable(captured.check());
    assert_eq!(binding.authority().unwrap().revision, revision(2));
    let rejected = binding.begin_refresh().unwrap();
    let duplicate = PolicyRefresh {
        binding: binding.clone(),
        ticket: rejected.ticket,
    };
    assert_conflict(rejected.install(configured(1)));
    assert_unavailable(duplicate.install(configured(2)));
    assert_eq!(binding.authority().unwrap().revision, revision(2));
}

#[test]
fn configuration_and_lease_modes_require_explicit_replacement() {
    let (policies, configured_binding) = registry();
    install(&configured_binding, configured(1));
    let until = Instant::now() + Duration::from_secs(60);
    assert_conflict(
        configured_binding
            .begin_refresh()
            .unwrap()
            .install(leased(1, until)),
    );
    assert_conflict(
        configured_binding
            .begin_refresh()
            .unwrap()
            .install(leased(2, until)),
    );
    configured_binding.authority().unwrap().check().unwrap();
    let leased_binding = policies.bind(configured_binding.app_id().clone()).unwrap();
    install(&leased_binding, leased(1, until));
    assert_conflict(
        leased_binding
            .begin_refresh()
            .unwrap()
            .install(configured(1)),
    );
    assert_conflict(
        leased_binding
            .begin_refresh()
            .unwrap()
            .install(configured(2)),
    );
    leased_binding.authority().unwrap().check().unwrap();
}

#[test]
fn shortening_then_extension_never_revives_captured_authority() {
    let (_, binding) = registry();
    let now = Instant::now();
    let original_deadline = now + Duration::from_secs(30);
    install(&binding, leased(1, original_deadline));
    let captured = binding.authority().unwrap();
    install(&binding, leased(1, now + Duration::from_secs(60)));
    captured.check().unwrap();
    assert_eq!(captured.deadline, Some(original_deadline));
    let before_shortening = binding.authority().unwrap();
    let stale_extension = binding.begin_refresh().unwrap();
    install(&binding, leased(1, now + Duration::from_secs(10)));
    let narrow = binding.authority().unwrap();
    assert_unavailable(captured.check());
    assert_unavailable(before_shortening.check());
    assert_unavailable(stale_extension.install(leased(1, now + Duration::from_secs(90))));
    install(&binding, leased(1, now + Duration::from_secs(120)));
    assert_unavailable(captured.check());
    assert_unavailable(before_shortening.check());
    narrow.check().unwrap();
    assert_eq!(narrow.deadline, Some(now + Duration::from_secs(10)));
    assert!(narrow.effective().unwrap().lease_ms <= 10_000);
    assert!(binding.authority().unwrap().effective().unwrap().lease_ms > 10_000);
    assert_eq!(
        binding.authority().unwrap().deadline,
        Some(now + Duration::from_secs(120))
    );
}

#[test]
fn expired_snapshots_narrow_authority_and_fresh_refreshes_serve_new_operations() {
    let (policies, binding) = registry();
    let expired = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    install(&binding, leased(1, expired));
    assert_unavailable(binding.authority());
    let effective = binding.resolve().unwrap();
    assert!(!effective.admission && !effective.dispatch && !effective.ingress);
    assert_eq!(policies.resolve(binding.app_id()).unwrap(), effective);
    assert_eq!(policies.app_ids().unwrap(), [binding.app_id().clone()]);
    let deadline = Instant::now() + Duration::from_secs(60);
    install(&binding, leased(1, deadline));
    let captured = binding.authority().unwrap();
    captured.check().unwrap();
    install(&binding, leased(2, expired));
    assert_unavailable(captured.check());
    assert_unavailable(binding.authority());
    install(&binding, leased(2, deadline));
    assert_unavailable(captured.check());
    binding.authority().unwrap().check().unwrap();
    binding.revoke().unwrap();
    assert_unavailable(binding.begin_refresh());
}

#[test]
fn identity_counters_fail_without_reuse_or_replacing_live_authority() {
    let (policies, binding) = registry();
    install(&binding, configured(1));
    let authority = binding.authority().unwrap();
    policies.state.write().unwrap().generation = u64::MAX;
    assert_unavailable(policies.bind(binding.app_id().clone()));
    authority.check().unwrap();
    let current = binding.begin_refresh().unwrap();
    {
        let mut state = policies.state.write().unwrap();
        state
            .entries
            .get_mut(binding.app_id())
            .unwrap()
            .current
            .as_mut()
            .unwrap()
            .ticket_sequence = u64::MAX;
    }
    assert_unavailable(binding.begin_refresh());
    current.install(configured(1)).unwrap();
    authority.check().unwrap();
    {
        let mut state = policies.state.write().unwrap();
        let current = state
            .entries
            .get_mut(binding.app_id())
            .unwrap()
            .current
            .as_mut()
            .unwrap();
        current.ticket_sequence = 10;
        current.epoch.number = u64::MAX;
        drop(state);
    }
    let authority = binding.authority().unwrap();
    assert_unavailable(binding.begin_refresh().unwrap().install(configured(2)));
    authority.check().unwrap();
    assert_eq!(binding.authority().unwrap().revision, revision(1));
}

struct LockObserver {
    registry: Arc<HostPolicies>,
    woke: AtomicBool,
    unlocked: AtomicBool,
}
impl ArcWake for LockObserver {
    fn wake_by_ref(this: &Arc<Self>) {
        this.unlocked
            .store(this.registry.state.try_read().is_ok(), Ordering::SeqCst);
        this.woke.store(true, Ordering::SeqCst);
    }
}

#[test]
fn invalidation_wakes_after_registry_lock_is_released() {
    let (policies, binding) = registry();
    install(&binding, configured(1));
    let authority = binding.authority().unwrap();
    let mut cancellation = Box::pin(authority.cancelled());
    let observer = Arc::new(LockObserver {
        registry: policies,
        woke: AtomicBool::new(false),
        unlocked: AtomicBool::new(false),
    });
    let waker = futures::task::waker(Arc::clone(&observer));
    let mut context = Context::from_waker(&waker);
    assert!(cancellation.as_mut().poll(&mut context).is_pending());
    install(&binding, configured(2));
    assert!(observer.woke.load(Ordering::SeqCst));
    assert!(observer.unlocked.load(Ordering::SeqCst));
    assert_eq!(cancellation.as_mut().poll(&mut context), Poll::Ready(()));
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
enum Change {
    Rebind,
    Revoke,
    Shorten,
    Revision,
}

#[compio::test]
async fn invalidation_cancels_pending_operations_and_drops_their_waits() {
    for change in [
        Change::Rebind,
        Change::Revoke,
        Change::Shorten,
        Change::Revision,
    ] {
        let (policies, binding) = registry();
        let deadline = Instant::now() + Duration::from_secs(60);
        install(&binding, leased(1, deadline));
        let captured = CapturedPolicy::capture(&binding);
        let (entered, waiting) = oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let drop_guard = Dropped(Arc::clone(&dropped));
        let operation = async move {
            let _guard = drop_guard;
            entered.send(()).unwrap();
            pending::<Result<(), WorkflowServiceError>>().await
        };
        let invalidate = async {
            waiting.await.unwrap();
            match change {
                Change::Rebind => {
                    policies.bind(binding.app_id().clone()).unwrap();
                }
                Change::Revoke => binding.revoke().unwrap(),
                Change::Shorten => install(
                    &binding,
                    leased(1, deadline.checked_sub(Duration::from_secs(30)).unwrap()),
                ),
                Change::Revision => install(&binding, leased(2, deadline)),
            }
        };
        let (result, ()) = compio::time::timeout(Duration::from_secs(2), async {
            futures::join!(captured.run(operation), invalidate)
        })
        .await
        .unwrap();
        assert_unavailable(result);
        assert_unavailable(captured.check());
        assert_unavailable(captured.recheck());
        assert!(dropped.load(Ordering::SeqCst));
    }
}

#[compio::test]
async fn operation_result_rechecks_invalidation_and_keeps_its_original_deadline() {
    let (_, binding) = registry();
    install(&binding, configured(1));
    let authority = binding.authority().unwrap();
    let result = authority
        .run(async {
            binding.revoke().unwrap();
            Ok(())
        })
        .await;
    assert_unavailable(result);

    let (_, binding) = registry();
    let original = Instant::now() + Duration::from_millis(30);
    install(&binding, leased(1, original));
    let authority = binding.authority().unwrap();
    let result = compio::time::timeout(
        Duration::from_secs(2),
        authority.run(async {
            install(
                &binding,
                leased(1, Instant::now() + Duration::from_secs(60)),
            );
            pending::<Result<(), WorkflowServiceError>>().await
        }),
    )
    .await
    .unwrap();
    assert_unavailable(result);
    assert_unavailable(authority.check());
    binding.authority().unwrap().check().unwrap();
}

#[compio::test]
async fn initial_missing_authority_permits_receipt_reads_but_never_mutation_authority() {
    let (_, binding) = registry();
    let captured = CapturedPolicy::capture(&binding);
    captured.recheck().unwrap();
    assert_unavailable(captured.check());
    assert_unavailable(captured.authority());
    assert_eq!(
        captured
            .run(async { Ok("retained-receipt") })
            .await
            .unwrap(),
        "retained-receipt"
    );
    install(&binding, configured(1));
    assert_unavailable(captured.check());
    CapturedPolicy::capture(&binding).check().unwrap();
}

/// A renewal of the SAME window may land slightly earlier than the one before
/// it, because the deadline is rebuilt from a duration measured across a round
/// trip and anchored at the near end. That is an artefact of the anchoring, not
/// a reduction of authority, and retiring the epoch for it cancels every
/// delivery in flight - measured at 137 retirements in a single probe run,
/// every one of them this.
///
/// The control is the case one line down: earlier by MORE than the slack is a
/// real shortening and must still fence, which is the contract
/// `shortening_then_extension_never_revives_captured_authority` holds and which
/// an earlier attempt at this broke by clamping the deadline instead.
#[test]
fn a_deadline_re_anchored_inside_its_slack_keeps_the_epoch() {
    let (_, binding) = registry();
    let now = Instant::now();
    let granted = now + Duration::from_secs(30);
    install(&binding, leased(1, granted));
    let captured = binding.authority().unwrap();

    let slack = Duration::from_millis(40);
    install(
        &binding,
        leased(1, granted - Duration::from_millis(5)).with_anchor_slack(slack),
    );
    captured
        .check()
        .expect("a re-anchored deadline inside its slack must not retire the epoch");

    // A whole second earlier than the deadline now installed: far outside any
    // round trip, so this is a real reduction of the window and must fence even
    // though it carries the same slack.
    install(
        &binding,
        leased(1, granted - Duration::from_secs(1)).with_anchor_slack(slack),
    );
    assert_unavailable(captured.check());
}
