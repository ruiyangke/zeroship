use super::*;
use crate::{
    consumer::ConsumerOptions,
    delivery::{DeliveryOptions, JobTransport},
    prepared::{CreatorRuntime, PreparedOptions},
};
use futures::{channel::oneshot, future::Either};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    pin::Pin,
    rc::Rc,
    time::Duration,
};
use zeroship_core::app_id::AppId;

mod fixture;
use fixture::{batch, peer, Call, Fixture, Reply};

fn options() -> HostOptions {
    HostOptions {
        consumer: ConsumerOptions {
            slots: 2,
            idle_poll: Duration::from_hours(1),
            error_backoff: Duration::from_hours(1),
            drain: Duration::ZERO,
            delivery: DeliveryOptions {
                execution_timeout: Duration::from_secs(5),
                operation_timeout: Duration::from_secs(5),
                retry_delay: Duration::from_secs(1),
            },
        },
        prepared: PreparedOptions {
            capacity: 2,
            operation_timeout: Duration::from_secs(5),
        },
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

/// The host claims its zone over the enrolled, authenticated transport: one
/// batch for every free slot, with no cursor and no exclusion yet. A stopped
/// host cannot run again, and draining it stays possible.
#[compio::test]
async fn the_host_claims_its_zone_for_every_free_slot() {
    let fixture = Fixture::new();
    let (claimed, observed) = oneshot::channel();
    let mut claimed = Some(claimed);
    peer(
        &fixture,
        |call| match call {
            Call::Claim(_) => {
                let reply = batch(&[], true);
                match claimed.take() {
                    Some(claimed) => reply.sent(claimed),
                    None => reply,
                }
            }
            Call::Release(_) => panic!("an empty zone gives nothing back"),
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
            let claims = fixture.claims();
            assert_eq!(claims.len(), 1, "an idle host claimed again before its interval");
            assert_eq!(claims[0].max.get(), 2);
            assert_eq!(claims[0].after, None);
            assert!(claims[0].exclude.is_empty());
            assert!(matches!(
                host.run_until(futures::future::pending()).await,
                Err(WorkflowServiceError::Conflict(_))
            ));
            host.drain().await;
            assert_eq!(fixture.claims().len(), 1);
        },
    )
    .await;
}

/// A delivery whose app cannot be prepared goes back over the release route,
/// carrying the journal task its acceptance leased and the reason the service
/// returns the row for; the next claim excludes the app.
#[compio::test]
async fn a_failed_preparation_releases_the_delivery_with_its_task_and_reason() {
    let fixture = Fixture::new();
    let app = AppId::mint();
    let (delivery, assignment, delivered) = fixture.delivery(&app);
    let (excluded, observed) = oneshot::channel();
    let mut excluded = Some(excluded);
    let mut delivered = Some(delivered);
    peer(
        &fixture,
        |call| match call {
            Call::Claim(request) => {
                let reply = batch(&delivered.take().into_iter().collect::<Vec<_>>(), true);
                match (request.exclude.is_empty(), excluded.take()) {
                    (false, Some(excluded)) => reply.sent(excluded),
                    (_, unsent) => {
                        excluded = unsent;
                        reply
                    }
                }
            }
            Call::Release(_) => Reply::ok(Value::Null),
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
            assert_eq!(fixture.opened.get(), 1);
            let releases = fixture.releases();
            assert_eq!(releases.len(), 1);
            assert_eq!(releases[0]["delivery"], json!(delivery));
            assert_eq!(releases[0]["reason"], json!("preparation_failed"));
            assert_eq!(releases[0]["task"]["id"], json!(assignment.id));
            assert_eq!(releases[0]["task"]["token"], json!(assignment.token));
            let claims = fixture.claims();
            assert_eq!(claims.len(), 2);
            assert_eq!(claims[1].exclude, [app]);
            assert_eq!(claims[1].max.get(), 2, "the given-back delivery freed its slot");
        },
    )
    .await;
}

/// A release on the execution path reports the attempt as interrupted, so the
/// service counts it and claims it again at once. The control is the
/// preparation failure above, which the host gives back as `preparation_failed`
/// through the other transport call.
#[compio::test]
async fn an_execution_release_reports_an_interrupted_attempt() {
    let fixture = Fixture::new();
    let app = AppId::mint();
    let (delivery, assignment, delivered) = fixture.delivery(&app);
    let mut delivered = Some(delivered);
    peer(
        &fixture,
        |call| match call {
            Call::Claim(_) => batch(&delivered.take().into_iter().collect::<Vec<_>>(), true),
            Call::Release(_) => Reply::ok(Value::Null),
        },
        async |client| {
            let request = zeroship_core::workflow_jobs::ClaimJobs {
                max: 1.try_into().unwrap(),
                wait_ms: 5_000.try_into().unwrap(),
                after: None,
                exclude: Vec::new(),
            };
            let mut claimed = JobTransport::claim(&client, &request).await.unwrap();
            let claimed = claimed.deliveries.pop().expect("the peer delivered one job");
            let task = claimed.task().expect("the journal accepted execution").clone();
            JobTransport::release(&client, &(), &claimed.lease, &task)
                .await
                .unwrap();
            let releases = fixture.releases();
            assert_eq!(releases.len(), 1);
            assert_eq!(releases[0]["delivery"], json!(delivery));
            assert_eq!(releases[0]["reason"], json!("interrupted"));
            assert_eq!(releases[0]["task"]["id"], json!(assignment.id));
        },
    )
    .await;
}

/// A batch holding one delivery whose journal task arrived spent, and one whose
/// lease did, runs the rest: the spent task's delivery alone is given back,
/// with no task, as `spent`; the spent lease can be neither renewed nor given
/// back and is left to lapse. The control is the other delivery of the same
/// reply, which is handed on.
#[compio::test]
async fn a_spent_task_is_given_back_alone_and_the_rest_of_the_batch_runs() {
    let fixture = Fixture::new();
    let (live, _, live_body) = fixture.delivery(&AppId::mint());
    let (spent, _, mut spent_body) = fixture.delivery(&AppId::mint());
    let accepted = spent_body["accepted"].as_object_mut().unwrap();
    let budget = if accepted.contains_key("remainingMs") {
        "remainingMs"
    } else {
        "remaining_ms"
    };
    accepted.insert(budget.to_owned(), json!(1));
    let (_, _, mut lapsed_body) = fixture.delivery(&AppId::mint());
    lapsed_body["lease"]["remainingMs"] = json!(1);
    let mut reply = Some(vec![live_body, spent_body, lapsed_body]);
    peer(
        &fixture,
        |call| match call {
            Call::Claim(_) => {
                // Held until the spent budgets' single millisecond has passed
                // since the claim reached this peer, and so since it was sent.
                let arrived = std::time::Instant::now();
                let (open, gate) = oneshot::channel();
                compio::runtime::spawn(async move {
                    while std::time::Instant::now() <= arrived + Duration::from_millis(2) {
                        compio::time::sleep(Duration::from_millis(1)).await;
                    }
                    let _ = open.send(());
                })
                .detach();
                batch(&reply.take().unwrap_or_default(), true).gated(gate)
            }
            Call::Release(_) => Reply::ok(Value::Null),
        },
        async |client| {
            let request = zeroship_core::workflow_jobs::ClaimJobs {
                max: 3.try_into().unwrap(),
                wait_ms: 5_000.try_into().unwrap(),
                after: None,
                exclude: Vec::new(),
            };
            let claimed = JobTransport::claim(&client, &request).await.unwrap();
            let handed: Vec<_> = claimed
                .deliveries
                .iter()
                .map(|claimed| claimed.lease.delivery().clone())
                .collect();
            assert_eq!(handed, std::slice::from_ref(&live), "only the live delivery is handed on");
            let releases = fixture.releases();
            assert_eq!(releases.len(), 1, "{releases:?}");
            assert_eq!(releases[0]["delivery"], json!(spent));
            assert_eq!(releases[0]["reason"], json!("unsent"));
            assert_eq!(releases[0]["task"], Value::Null);
        },
    )
    .await;
}

#[compio::test]
async fn dropping_run_requires_drain_and_permanently_rejects_restart() {
    let fixture = Fixture::new();
    let (arrived, observed) = oneshot::channel();
    let (finish_claim, blocked_claim) = oneshot::channel();
    let mut arrived = Some(arrived);
    let mut blocked_claim = Some(blocked_claim);
    peer(
        &fixture,
        |call| match call {
            Call::Claim(_) => {
                arrived.take().unwrap().send(()).unwrap();
                batch(&[], true)
                    .gated(blocked_claim.take().unwrap())
                    .abandoned()
            }
            Call::Release(_) => panic!("a pending claim gives nothing back"),
        },
        async |client| {
            let mut host = fixture.host(client, options());
            let mut running = Box::pin(host.run_until(futures::future::pending()));
            reached(observed, running.as_mut()).await;
            drop(running);
            assert!(host.run_until(futures::future::pending()).await.is_err());
            host.drain().await;
            host.drain().await;
            assert!(host.run_until(futures::future::pending()).await.is_err());
            assert_eq!(fixture.claims().len(), 1);
            finish_claim.send(()).unwrap();
        },
    )
    .await;
}

/// Bounds are refused at construction, before any network I/O, including a
/// prepared-app cache smaller than the slots it serves.
#[compio::test]
async fn the_host_refuses_invalid_bounds_before_network_io() {
    let fixture = Fixture::new();
    peer(
        &fixture,
        |_| panic!("construction must not perform network I/O"),
        async |client| {
            let valid = options();
            for invalid in [
                HostOptions {
                    prepared: PreparedOptions {
                        capacity: valid.consumer.slots - 1,
                        ..valid.prepared
                    },
                    ..valid
                },
                HostOptions {
                    consumer: ConsumerOptions {
                        slots: 0,
                        ..valid.consumer
                    },
                    ..valid
                },
                HostOptions {
                    consumer: ConsumerOptions {
                        idle_poll: Duration::ZERO,
                        ..valid.consumer
                    },
                    ..valid
                },
                HostOptions {
                    prepared: PreparedOptions {
                        operation_timeout: Duration::MAX,
                        ..valid.prepared
                    },
                    ..valid
                },
            ] {
                assert!(matches!(
                    fixture.try_host(client.clone(), invalid),
                    Err(WorkflowServiceError::InvalidRequest(_))
                ));
            }
            let host = fixture.host(client, valid);
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

/// Bytes the kernel gave the stack of a thread this builder spawns.
///
/// Read from the thread's own attributes. The kernel merges adjacent mappings
/// that share permissions, so the `/proc/self/maps` range holding a stack
/// address can span unrelated memory and answers "large" for any thread.
#[expect(
    unsafe_code,
    reason = "the thread's own stack attributes are only readable through pthread"
)]
fn mapped_stack_bytes(builder: std::thread::Builder) -> Option<usize> {
    builder
        .spawn(|| {
            // SAFETY: `pthread_getattr_np` fills an attribute object for the
            // calling thread and `pthread_attr_getstack` reads the base and
            // size recorded in it. Both receive pointers to locals that outlive
            // the call, and the attribute object is destroyed before return.
            unsafe {
                let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
                if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
                    return None;
                }
                let mut attr = attr.assume_init();
                let mut base = std::ptr::null_mut();
                let mut size = 0usize;
                let read = libc::pthread_attr_getstack(&raw const attr, &raw mut base, &raw mut size);
                libc::pthread_attr_destroy(&raw mut attr);
                (read == 0).then_some(size)
            }
        })
        .ok()?
        .join()
        .ok()?
}
