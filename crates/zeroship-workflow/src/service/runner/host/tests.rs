use super::*;
use crate::service::{
    reconciliation::ReconciliationOptions,
    runner::{
        assignments::{AssignmentOptions, CreatorFactory, CreatorRuntime},
        consumer::ConsumerOptions,
        delivery::DeliveryOptions,
    },
    PolicyBinding,
};
use futures::{channel::oneshot, future::Either, FutureExt};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    pin::Pin,
    rc::Rc,
};
use zeroship_core::workflow_coordination::{AssignedScope, RegisterWorker, WorkerState};

mod fixture;
use fixture::{peer, Call, Fixture, Reply, UnexpectedCreator};

fn options() -> HostOptions {
    HostOptions {
        consumer: ConsumerOptions {
            slots: 1,
            max_scopes: 7,
            idle_poll: Duration::from_secs(1),
            error_backoff: Duration::from_secs(1),
            delivery: DeliveryOptions {
                execution_timeout: Duration::from_secs(5),
                operation_timeout: Duration::from_secs(5),
                retry_delay: Duration::from_secs(1),
                reconciliation: ReconciliationOptions::default(),
                collection: crate::service::collection::CollectionOptions::default(),
                fanout: crate::service::fanout::FanoutOptions::default(),
                propagation: crate::service::propagation::PropagationOptions::default(),
            },
        },
        assignments: AssignmentOptions {
            max_scopes: 3,
            operation_timeout: Duration::from_secs(5),
        },
        registration_interval: Duration::from_secs(3600),
        assignment_interval: Duration::from_secs(3600),
        policy_interval: Duration::from_secs(3600),
    }
}

async fn reached<F: Future>(observed: oneshot::Receiver<()>, running: Pin<&mut F>) {
    assert!(
        matches!(
            futures::future::select(observed, running).await,
            Either::Left((Ok(()), _))
        ),
        "host stopped before the expected exchange"
    );
}

#[compio::test]
async fn initial_registration_retries_before_scanning_and_advertises_scope_capacity() {
    let fixture = Fixture::new();
    let (accepted, observed) = oneshot::channel();
    let (release, blocked) = oneshot::channel();
    let (scanned, scan_observed) = oneshot::channel();
    let attempts = Cell::new(0);
    let permitted = Cell::new(false);
    let mut accepted = Some(accepted);
    let mut blocked = Some(blocked);
    let mut scanned = Some(scanned);
    let limits = HostOptions {
        registration_interval: Duration::from_millis(1),
        ..options()
    };
    peer(
        &fixture,
        |call| match call {
            Call::Register(request) => {
                assert_eq!(
                    usize::try_from(request.capacity.get()).unwrap(),
                    limits.assignments.max_scopes
                );
                assert_ne!(
                    usize::try_from(request.capacity.get()).unwrap(),
                    limits.consumer.slots
                );
                if request.state == WorkerState::Ready {
                    attempts.set(attempts.get() + 1);
                    match attempts.get() {
                        1 => Reply::failure(503, "unavailable"),
                        2 => {
                            accepted.take().unwrap().send(()).unwrap();
                            fixture.registration(request).gated(blocked.take().unwrap())
                        }
                        _ => fixture.registration(request),
                    }
                } else {
                    fixture.registration(request)
                }
            }
            Call::Assignments(page) => {
                assert!(
                    permitted.get(),
                    "assignments cannot precede successful Ready"
                );
                assert!(page.after.is_none());
                scanned.take().map_or_else(
                    || Reply::ok(json!([])),
                    |scanned| Reply::ok(json!([])).sent(scanned),
                )
            }
        },
        async |client| {
            let mut host = fixture.host(client, limits);
            let (stop, stopped) = oneshot::channel();
            let mut running = Box::pin(host.run_until(async {
                stopped.await.unwrap();
            }));
            reached(observed, running.as_mut()).await;
            assert_eq!(fixture.registrations().len(), 2);
            assert!(fixture
                .calls
                .borrow()
                .iter()
                .all(|call| matches!(call, Call::Register(_))));
            permitted.set(true);
            release.send(()).unwrap();
            reached(scan_observed, running.as_mut()).await;
            stop.send(()).unwrap();
            running.await.unwrap();
            assert_eq!(
                fixture.registrations().last().unwrap().state,
                WorkerState::Draining
            );
            host.drain().await.unwrap();
        },
    )
    .await;
}

#[compio::test]
async fn timed_out_initial_registration_is_cancelled_and_retried() {
    let mut fixture = Fixture::new();
    fixture.client_options.timeout = Duration::from_millis(250);
    let (finish_old, old_blocked) = oneshot::channel();
    let (scanned, scan_observed) = oneshot::channel();
    let attempts = Cell::new(0);
    let mut old_blocked = Some(old_blocked);
    let mut scanned = Some(scanned);
    let limits = HostOptions {
        registration_interval: Duration::from_millis(1),
        ..options()
    };
    peer(
        &fixture,
        |call| match call {
            Call::Register(request) if request.state == WorkerState::Ready => {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    fixture
                        .registration(request)
                        .gated(old_blocked.take().unwrap())
                        .abandoned()
                } else {
                    fixture.registration(request)
                }
            }
            Call::Register(request) => fixture.registration(request),
            Call::Assignments(_) => {
                assert!(attempts.get() > 1);
                scanned.take().map_or_else(
                    || Reply::ok(json!([])),
                    |scanned| Reply::ok(json!([])).sent(scanned),
                )
            }
        },
        async |client| {
            let mut host = fixture.host(client, limits);
            let (stop, stopped) = oneshot::channel();
            let mut running = Box::pin(host.run_until(async {
                stopped.await.unwrap();
            }));
            reached(scan_observed, running.as_mut()).await;
            stop.send(()).unwrap();
            running.await.unwrap();
            finish_old.send(()).unwrap();
            assert_eq!(
                fixture.registrations().last().unwrap().state,
                WorkerState::Draining
            );
        },
    )
    .await;
}

#[compio::test]
async fn refused_or_malformed_initial_registration_is_terminal() {
    for refusal in ["denied", "conflict", "malformed"] {
        let fixture = Fixture::new();
        peer(
            &fixture,
            |call| match call {
                Call::Register(request) if request.state == WorkerState::Ready => match refusal {
                    "denied" => Reply::failure(403, "denied"),
                    "conflict" => Reply::failure(409, "conflict"),
                    _ => Reply::ok(json!({"workerId":fixture.worker})),
                },
                Call::Register(request) => fixture.registration(request),
                Call::Assignments(_) => {
                    panic!("a rejected registration must never start placement or execution")
                }
            },
            async |client| {
                let mut host = fixture.host(client, options());
                let result = host.run_until(futures::future::pending()).await;
                assert!(result.is_err());
                let registered = fixture.registrations();
                assert_eq!(
                    registered
                        .iter()
                        .map(|request| request.state)
                        .collect::<Vec<_>>(),
                    vec![WorkerState::Ready, WorkerState::Draining]
                );
                assert!(host.run_until(futures::future::pending()).await.is_err());
                assert_eq!(fixture.registrations(), registered);
            },
        )
        .await;
    }
}

#[compio::test]
async fn periodic_registration_progresses_while_assignment_scan_waits() {
    let fixture = Fixture::new();
    let (scan, scan_observed) = oneshot::channel();
    let (finish_scan, blocked_scan) = oneshot::channel();
    let (renewed, renewal_observed) = oneshot::channel();
    let scan_pending = Cell::new(false);
    let mut scan = Some(scan);
    let mut blocked_scan = Some(blocked_scan);
    let mut renewed = Some(renewed);
    let limits = HostOptions {
        registration_interval: Duration::from_millis(1),
        ..options()
    };
    peer(
        &fixture,
        |call| match call {
            Call::Register(request) => {
                if request.state == WorkerState::Ready && scan_pending.get() {
                    if let Some(renewed) = renewed.take() {
                        return fixture.registration(request).sent(renewed);
                    }
                }
                fixture.registration(request)
            }
            Call::Assignments(_) => {
                assert!(
                    !scan_pending.replace(true),
                    "a pending scan must not be duplicated"
                );
                scan.take().unwrap().send(()).unwrap();
                Reply::ok(json!([]))
                    .gated(blocked_scan.take().unwrap())
                    .abandoned()
            }
        },
        async |client| {
            let mut host = fixture.host(client, limits);
            let (stop, stopped) = oneshot::channel();
            let mut running = Box::pin(host.run_until(async {
                stopped.await.unwrap();
            }));
            reached(scan_observed, running.as_mut()).await;
            reached(renewal_observed, running.as_mut()).await;
            assert!(
                fixture
                    .registrations()
                    .iter()
                    .filter(|request| request.state == WorkerState::Ready)
                    .count()
                    > 1
            );
            stop.send(()).unwrap();
            running.await.unwrap();
            finish_scan.send(()).unwrap();
            assert_eq!(
                fixture.registrations().last().unwrap().state,
                WorkerState::Draining
            );
        },
    )
    .await;
}

#[compio::test]
async fn shutdown_cancels_in_flight_ready_before_draining() {
    let fixture = Fixture::new();
    let (arrived, observed) = oneshot::channel();
    let (finish_ready, blocked_ready) = oneshot::channel();
    let mut arrived = Some(arrived);
    let mut blocked_ready = Some(blocked_ready);
    peer(
        &fixture,
        |call| match call {
            Call::Register(request) if request.state == WorkerState::Ready => {
                arrived.take().unwrap().send(()).unwrap();
                fixture
                    .registration(request)
                    .gated(blocked_ready.take().unwrap())
                    .abandoned()
            }
            Call::Register(request) => fixture.registration(request),
            Call::Assignments(_) => panic!("pending Ready cannot authorize other work"),
        },
        async |client| {
            let mut host = fixture.host(client, options());
            let (stop, stopped) = oneshot::channel();
            let mut running = Box::pin(host.run_until(async {
                stopped.await.unwrap();
            }));
            reached(observed, running.as_mut()).await;
            stop.send(()).unwrap();
            running.await.unwrap();
            finish_ready.send(()).unwrap();
            assert_eq!(
                fixture
                    .registrations()
                    .iter()
                    .map(|request| request.state)
                    .collect::<Vec<_>>(),
                vec![WorkerState::Ready, WorkerState::Draining]
            );
        },
    )
    .await;
}

#[compio::test]
async fn dropping_run_requires_drain_and_permanently_rejects_restart() {
    let fixture = Fixture::new();
    let (arrived, observed) = oneshot::channel();
    let (finish_ready, blocked_ready) = oneshot::channel();
    let mut arrived = Some(arrived);
    let mut blocked_ready = Some(blocked_ready);
    peer(
        &fixture,
        |call| match call {
            Call::Register(request) if request.state == WorkerState::Ready => {
                arrived.take().unwrap().send(()).unwrap();
                fixture
                    .registration(request)
                    .gated(blocked_ready.take().unwrap())
                    .abandoned()
            }
            Call::Register(request) => fixture.registration(request),
            Call::Assignments(_) => panic!("pending registration cannot authorize a creator"),
        },
        async |client| {
            let mut host = fixture.host(client, options());
            let mut running = Box::pin(host.run_until(futures::future::pending()));
            reached(observed, running.as_mut()).await;
            drop(running);
            assert!(host.run_until(futures::future::pending()).await.is_err());
            host.drain().await.unwrap();
            let registered = fixture.registrations();
            assert_eq!(
                registered
                    .iter()
                    .map(|request| request.state)
                    .collect::<Vec<_>>(),
                vec![WorkerState::Ready, WorkerState::Draining]
            );
            host.drain().await.unwrap();
            let drained_again = fixture.registrations();
            assert_eq!(&drained_again[..registered.len()], &registered);
            assert_eq!(drained_again.len(), registered.len() + 1);
            assert_eq!(drained_again.last().unwrap().state, WorkerState::Draining);
            assert!(host.run_until(futures::future::pending()).await.is_err());
            assert_eq!(fixture.registrations(), drained_again);
            finish_ready.send(()).unwrap();
        },
    )
    .await;
}

#[compio::test]
async fn already_ready_shutdown_sends_only_a_draining_tombstone() {
    let fixture = Fixture::new();
    peer(
        &fixture,
        |call| match call {
            Call::Register(request) if request.state == WorkerState::Draining => {
                fixture.registration(request)
            }
            _ => panic!("an already stopped host cannot become Ready"),
        },
        async |client| {
            let mut host = fixture.host(client, options());
            host.run_until(futures::future::ready(())).await.unwrap();
            assert_eq!(
                fixture
                    .registrations()
                    .iter()
                    .map(|request| request.state)
                    .collect::<Vec<_>>(),
                vec![WorkerState::Draining]
            );
            assert!(host.run_until(futures::future::pending()).await.is_err());
        },
    )
    .await;
}

#[compio::test]
async fn failed_draining_is_reported_and_local_host_stays_terminal() {
    let fixture = Fixture::new();
    peer(
        &fixture,
        |call| match call {
            Call::Register(request) if request.state == WorkerState::Draining => {
                Reply::failure(503, "unavailable")
            }
            _ => panic!("shutdown cannot authorize Ready"),
        },
        async |client| {
            let mut host = fixture.host(client, options());
            assert!(matches!(
                host.run_until(futures::future::ready(())).await,
                Err(WorkflowServiceError::Unavailable(_))
            ));
            assert!(host.run_until(futures::future::pending()).await.is_err());
            assert_eq!(fixture.registrations().len(), 1);
        },
    )
    .await;
}

#[compio::test]
async fn host_rejects_invalid_intervals_before_network_io() {
    let fixture = Fixture::new();
    peer(
        &fixture,
        |_| panic!("construction must not perform network I/O"),
        async |client| {
            for invalid in [
                HostOptions {
                    registration_interval: Duration::ZERO,
                    ..options()
                },
                HostOptions {
                    assignment_interval: Duration::ZERO,
                    ..options()
                },
                HostOptions {
                    policy_interval: Duration::ZERO,
                    ..options()
                },
                HostOptions {
                    registration_interval: Duration::MAX,
                    ..options()
                },
                HostOptions {
                    assignment_interval: Duration::MAX,
                    ..options()
                },
                HostOptions {
                    policy_interval: Duration::MAX,
                    ..options()
                },
            ] {
                assert!(matches!(
                    WorkerHost::new(
                        client.clone(),
                        fixture.policies.clone(),
                        UnexpectedCreator,
                        ReadyApps::default(),
                        invalid
                    ),
                    Err(WorkflowServiceError::InvalidRequest(_))
                ));
            }
            let host = fixture.host(client, options());
            drop(host);
            assert!(fixture.calls.borrow().is_empty());
        },
    )
    .await;
}

/// The host thread runs on the declared stack budget, not the platform default.
///
/// Measured from the mapping the kernel actually gave the thread, because the
/// builder's setting is a request and what the call chain can spend before the
/// process aborts is the mapping. The control thread asks for a small stack
/// explicitly, so the reading has to discriminate rather than answer "large"
/// for anything; asking for it explicitly also keeps the control out of reach
/// of whatever default the surrounding process was started with.
#[test]
fn the_host_thread_runs_on_the_declared_stack_budget() {
    let budget = mapped_stack_bytes(thread()).expect("host thread stack mapping");
    let control = mapped_stack_bytes(std::thread::Builder::new().stack_size(1024 * 1024))
        .expect("control thread stack mapping");
    assert!(
        control < STACK_BYTES / 2,
        "control thread measured {control} bytes, so this reading does not discriminate"
    );
    // A guard page and page rounding are the only shortfall the kernel may
    // impose on a requested stack.
    assert!(
        budget + 64 * 1024 >= STACK_BYTES,
        "host thread received {budget} bytes of stack, under its declared budget"
    );
}

/// Bytes the kernel mapped for the stack of a thread this builder spawns.
fn mapped_stack_bytes(builder: std::thread::Builder) -> Option<usize> {
    builder
        .spawn(|| {
            let anchor = 0u8;
            let address = std::ptr::addr_of!(anchor) as usize;
            let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
            maps.lines().find_map(|line| {
                let (range, _) = line.split_once(' ')?;
                let (start, end) = range.split_once('-')?;
                let start = usize::from_str_radix(start, 16).ok()?;
                let end = usize::from_str_radix(end, 16).ok()?;
                (start..end).contains(&address).then_some(end - start)
            })
        })
        .ok()?
        .join()
        .ok()?
}
