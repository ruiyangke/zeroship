//! The declarative per-zone capacity target, on one eligibility predicate.
#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native capacity fixtures stay on their compio runtime"
)]

#[allow(dead_code, reason = "shared fixtures expose other manager contracts")]
mod support;
#[allow(dead_code, reason = "shared placement fixtures serve the placement suite too")]
#[path = "support/placement.rs"]
mod placement_support;

use futures::{channel::oneshot, future::ready};
use placement_support::{
    blocked_manager, options, revision, Counted, Facts, Host, Pool, Starter, Step, LONG,
};
use std::{rc::Rc, time::Duration};
use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{AssignedScope, ReleaseReason, ReleaseScope, RequestId},
    workflow_jobs::{DeploymentId, JobOperation},
};
use zeroship_data_orm::{
    orm::{Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    capacity::{
        self, Capacity, CapacityProvider, Exchange, Refusal, StaticPool, TargetState, Visit,
    },
    eligibility::ZoneId,
    recovery::Recovery,
};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            Box::pin($contract(&Fixture::new(Backend::Sqlite).await)).await;
        }
        #[compio::test]
        async fn $postgres() {
            Box::pin($contract(&Fixture::new(Backend::Postgres).await)).await;
        }
    };
}

case!(
    sqlite_a_due_duty_scales_from_zero_and_its_job_reaches_the_started_worker,
    postgres_a_due_duty_scales_from_zero_and_its_job_reaches_the_started_worker,
    scale_from_zero
);
case!(
    sqlite_an_exhausted_static_pool_is_durable_and_retried_without_duplicates,
    postgres_an_exhausted_static_pool_is_durable_and_retried_without_duplicates,
    exhaustion
);
case!(
    sqlite_racing_replicas_converge_on_one_revision_and_one_request,
    postgres_racing_replicas_converge_on_one_revision_and_one_request,
    convergence
);
case!(
    sqlite_the_target_is_computed_from_bounded_pages,
    postgres_the_target_is_computed_from_bounded_pages,
    bounded_pages
);
case!(
    sqlite_a_larger_target_shrinks_only_after_its_hold_down,
    postgres_a_larger_target_shrinks_only_after_its_hold_down,
    hold_down
);
case!(
    sqlite_a_stale_reply_cannot_overwrite_a_newer_target,
    postgres_a_stale_reply_cannot_overwrite_a_newer_target,
    stale_target
);
case!(
    sqlite_targets_demand_and_jobs_survive_provider_failure_and_restart,
    postgres_targets_demand_and_jobs_survive_provider_failure_and_restart,
    restart_target
);
case!(
    sqlite_racing_replicas_start_no_more_workers_than_unabsorbable_placements,
    postgres_racing_replicas_start_no_more_workers_than_unabsorbable_placements,
    coalesced_starts
);

fn capacity(host: &Host, provider: Rc<dyn CapacityProvider>, retry: Duration) -> Capacity {
    Capacity::new(host.coordinator.clone(), provider, options(retry).capacity).unwrap()
}

async fn target(capacity: &Capacity, zone: &ZoneId) -> capacity::Target {
    capacity.target(zone).await.unwrap().expect("zone target")
}

async fn rows(fixture: &Fixture, collection: &str, filter: Value) -> Vec<Value> {
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection(collection)
        .unwrap()
        .find(filter, value!({"limit":256}))
        .await
        .unwrap()
    else {
        panic!("expected rows");
    };
    rows
}

/// Make a zone's paced retry due without waiting out its interval.
async fn retry_now(fixture: &Fixture, zone: &ZoneId) {
    let updated = fixture
        .database()
        .await
        .collection("capacity_targets")
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":zone.as_str()}),
            patch: value!({"retry_at":0}),
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(updated, Output::Count(1)), "{updated:?}");
}

/// Two replicas over one database, sharing Control's facts.
async fn replicas(fixture: &Fixture) -> (Host, Host) {
    let facts = Rc::new(Facts::default());
    let first = Host::new(fixture, facts.clone()).await;
    let second = Host::new(fixture, facts).await;
    (first, second)
}

/// An app with a due recovery duty and a zone with no workers. The lane
/// records demand, the provider starts an enrolled worker, the lane places
/// the app on it, and that worker claims the Reconcile job. The control
/// differs only in the provider: a static pool starts nothing, records a
/// durable `pool_exhausted` refusal and leaves the job pending.
async fn scale_from_zero(fixture: &Fixture) {
    for started in [true, false] {
        let facts = Rc::new(Facts::default());
        let host = Host::new(fixture, facts.clone()).await;
        let zone = ZoneId::mint();
        let app = AppId::mint();
        facts.app(&app, &zone);
        Recovery::new(host.queue.clone(), options(LONG).recovery)
            .unwrap()
            .ensure(&app, &DeploymentId::mint(), revision(1))
            .await
            .unwrap();
        let pool = Pool::new(Starter::new(host.clone(), 4));
        let provider: Rc<dyn CapacityProvider> = if started {
            pool.clone()
        } else {
            Rc::new(StaticPool)
        };
        let mut driver = host.driver(LONG, provider);
        let first = driver.tick().await;
        for lane in [&first.reconciliation, &first.placement, &first.capacity] {
            assert!(lane.failures.is_empty() && lane.scan_error.is_none(), "{first:?}");
        }
        let recorded = target(driver.capacity(), &zone).await;
        assert_eq!((recorded.revision, recorded.desired), (1, 1));
        driver.tick().await;
        if started {
            assert_eq!(pool.starts(), 1);
            assert_eq!(pool.calls(), 1);
            let worker = pool.started.borrow()[0].1.clone();
            assert_eq!(host.placed(&worker).await, vec![app.clone()]);
            let assignment = host.coordinator.assignments(&worker, None).await.unwrap()[0].clone();
            let grant = host
                .coordinator
                .claim_job(
                    &worker,
                    &AssignedScope {
                        app_id: app.clone(),
                        assignment_revision: assignment.revision,
                    },
                    || ready(Ok(worker.clone())),
                )
                .await
                .unwrap()
                .expect("the started worker receives the recovery job");
            assert_eq!(grant.delivery().job.operation, JobOperation::Reconcile {});
            assert!(driver.capacity().demands(&zone).await.unwrap().is_empty());
            // Placing the app moved it from unplaced demand to a live
            // placement; the target did not change.
            let settled = target(driver.capacity(), &zone).await;
            assert_eq!((settled.revision, settled.state), (1, TargetState::Steady));
        } else {
            let refused = target(driver.capacity(), &zone).await;
            assert_eq!(refused.state, TargetState::Refused);
            assert_eq!(refused.refusal, Some(Refusal::PoolExhausted));
            assert_eq!(pool.starts(), 0);
            assert_eq!(
                driver.capacity().demands(&zone).await.unwrap(),
                vec![app.clone()]
            );
            let pending = rows(
                fixture,
                "jobs",
                value!({"app_id":app.as_str(),"operation_kind":"reconcile"}),
            )
            .await;
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0]["state"].as_str(), Some("ready"));
            assert!(rows(fixture, "assignments", value!({"app_id":app.as_str()}))
                .await
                .is_empty());
        }
    }
}

/// A static pool's refusal is durable. Racing replicas send no request while
/// it waits out its retry interval, and exactly one request once it is due.
/// The control is an operator scaling the pool: the next retry progresses.
async fn exhaustion(fixture: &Fixture) {
    let (a, b) = replicas(fixture).await;
    let zone = ZoneId::mint();
    let app = AppId::mint();
    a.due(&app, &zone).await;
    let pool = Counted::new(StaticPool);
    let mut first = a.driver(LONG, pool.clone());
    let mut second = b.driver(LONG, pool.clone());
    first.tick().await;
    assert_eq!(pool.calls(), 1);
    let refused = target(first.capacity(), &zone).await;
    assert_eq!(
        (refused.revision, refused.state, refused.refusal),
        (1, TargetState::Refused, Some(Refusal::PoolExhausted))
    );
    for _ in 0..3 {
        futures::join!(first.tick(), second.tick());
    }
    assert_eq!(pool.calls(), 1, "a refusal waits out its retry interval");
    assert_eq!(target(second.capacity(), &zone).await, refused);
    let job = rows(fixture, "jobs", value!({"app_id":app.as_str()})).await;
    assert_eq!(job.len(), 1);
    assert_eq!(job[0]["state"].as_str(), Some("ready"));

    retry_now(fixture, &zone).await;
    futures::join!(first.tick(), second.tick());
    assert_eq!(pool.calls(), 2, "exactly one replica claims the due retry");
    let retried = target(first.capacity(), &zone).await;
    assert_eq!(
        (retried.revision, retried.attempt, retried.state),
        (1, 2, TargetState::Refused)
    );

    let worker = a.worker(&zone, 4).await;
    first.tick().await;
    assert_eq!(a.placed(&worker).await, vec![app]);
    retry_now(fixture, &zone).await;
    first.tick().await;
    assert_eq!(pool.calls(), 3);
    let steady = target(first.capacity(), &zone).await;
    assert_eq!((steady.state, steady.refusal), (TargetState::Steady, None));
}

/// With demand fixed, two replicas racing the zone's reconciliation advance
/// one revision and send one request; the declarative provider starts only
/// the workers the slots need, and the workers then absorb every app.
async fn convergence(fixture: &Fixture) {
    let (a, b) = replicas(fixture).await;
    let zone = ZoneId::mint();
    let apps: Vec<AppId> = (0..5).map(|_| AppId::mint()).collect();
    for app in &apps {
        a.due(app, &zone).await;
    }
    let pool = Pool::new(Starter::new(a.clone(), 2));
    let (left, right) = (
        capacity(&a, pool.clone(), LONG),
        capacity(&b, pool.clone(), LONG),
    );
    for app in &apps {
        assert_eq!(left.visit(app).await.unwrap(), Visit::Unplaced(zone.clone()));
    }
    let (x, y) = futures::join!(left.reconcile(&zone), right.reconcile(&zone));
    assert!(
        matches!(
            (x.unwrap(), y.unwrap()),
            (Exchange::Applied, Exchange::Idle) | (Exchange::Idle, Exchange::Applied)
        ),
        "one replica requests"
    );
    let converged = target(&left, &zone).await;
    assert_eq!(
        (converged.revision, converged.desired, converged.attempt),
        (1, 5, 1)
    );
    assert_eq!(pool.calls(), 1);
    assert_eq!(pool.starts(), 3);

    let mut first = a.driver(LONG, pool.clone());
    let mut second = b.driver(LONG, pool.clone());
    for _ in 0..3 {
        futures::join!(first.tick(), second.tick());
    }
    for app in &apps {
        assert!(a.coordinator.owned(app).await.unwrap());
    }
    assert!(left.demands(&zone).await.unwrap().is_empty());
    assert_eq!(pool.calls(), 1);
    assert_eq!(pool.starts(), 3);
    assert_eq!(target(&right, &zone).await.revision, 1);
}

/// Demand accumulates across bounded lane pages. Each turn visits at most a
/// page, and the zone's target follows the committed demand rows without any
/// lane scanning every app.
async fn bounded_pages(fixture: &Fixture) {
    let (a, _) = replicas(fixture).await;
    let zone = ZoneId::mint();
    let apps: Vec<AppId> = (0..5).map(|_| AppId::mint()).collect();
    for app in &apps {
        a.due(app, &zone).await;
    }
    let pool = Counted::new(StaticPool);
    let mut driver = zeroship_workflow_manager::driver::Driver::new(
        a.coordinator.clone(),
        zeroship_workflow_manager::driver::Options {
            page_limit: 2,
            ..options(LONG)
        },
        std::rc::Rc::new(zeroship_workflow_manager::lifecycle::Undeletable),
        pool.clone(),
    )
    .unwrap();
    let mut desired = Vec::new();
    loop {
        let report = driver.tick().await;
        assert!(report.placement.visited <= 2, "{report:?}");
        assert!(report.capacity.visited <= 2, "{report:?}");
        let current = target(driver.capacity(), &zone).await;
        if desired.last() != Some(&current.desired) {
            desired.push(current.desired);
        }
        if report.placement.sweep_complete {
            break;
        }
        assert!(desired.len() <= apps.len(), "{desired:?}");
    }
    assert_eq!(desired.last(), Some(&5));
    assert_eq!(driver.capacity().demands(&zone).await.unwrap().len(), 5);
    let settled = target(driver.capacity(), &zone).await;
    driver.tick().await;
    assert_eq!(target(driver.capacity(), &zone).await.revision, settled.revision);
}

/// A lower target applies only after the hold-down, so capacity a larger
/// earlier target requested converges away instead of persisting. The control
/// differs only in the hold-down: under a long one the target keeps its size.
async fn hold_down(fixture: &Fixture) {
    let lowered = shrink(fixture, Duration::from_millis(1)).await;
    assert_eq!((lowered.revision, lowered.desired), (2, 2));
    let kept = shrink(fixture, LONG).await;
    assert_eq!((kept.revision, kept.desired), (1, 3));
}

/// Three apps placed on one-slot workers; one placement is then released and
/// the zone reconciled twice across the given hold-down.
async fn shrink(fixture: &Fixture, idle_hold_down: Duration) -> capacity::Target {
    let (a, _) = replicas(fixture).await;
    let zone = ZoneId::mint();
    let apps: Vec<AppId> = (0..3).map(|_| AppId::mint()).collect();
    for app in &apps {
        a.due(app, &zone).await;
    }
    let pool = Pool::new(Starter::new(a.clone(), 1));
    let lane = Capacity::new(
        a.coordinator.clone(),
        pool.clone(),
        capacity::Options {
            idle_hold_down,
            ..options(LONG).capacity
        },
    )
    .unwrap();
    for app in &apps {
        lane.visit(app).await.unwrap();
    }
    assert_eq!(lane.reconcile(&zone).await.unwrap(), Exchange::Applied);
    for app in &apps {
        assert!(matches!(lane.visit(app).await.unwrap(), Visit::Placed(_)));
    }
    assert_eq!(target(&lane, &zone).await.desired, 3);
    let workers: Vec<_> = pool.started.borrow().iter().map(|(_, id)| id.clone()).collect();
    let mut released = false;
    for worker in &workers {
        let Some(assignment) = a.coordinator.assignments(worker, None).await.unwrap().pop() else {
            continue;
        };
        a.coordinator
            .release(
                worker,
                &ReleaseScope {
                    request_id: RequestId::mint(),
                    app_id: assignment.app_id,
                    assignment_revision: assignment.revision,
                    reason: ReleaseReason::Relinquished,
                },
            )
            .await
            .unwrap();
        released = true;
        break;
    }
    assert!(released);
    // The first pass below the target only starts the hold-down.
    assert_eq!(lane.reconcile(&zone).await.unwrap(), Exchange::Idle);
    assert_eq!(target(&lane, &zone).await.desired, 3);
    compio::time::sleep(Duration::from_millis(5)).await;
    let _ = lane.reconcile(&zone).await.unwrap();
    target(&lane, &zone).await
}

/// A reply delayed past a newer revision is stale: it cannot replace the
/// newer target's state.
async fn stale_target(fixture: &Fixture) {
    let (a, b) = replicas(fixture).await;
    let zone = ZoneId::mint();
    let (first_app, second_app) = (AppId::mint(), AppId::mint());
    a.due(&first_app, &zone).await;
    let pool = Pool::new(Starter::new(a.clone(), 1));
    let (entered, arrived) = oneshot::channel();
    let (release, released) = oneshot::channel();
    pool.script.borrow_mut().push_back(Step::Gate {
        entered,
        release: released,
    });
    let (left, right) = (
        capacity(&a, pool.clone(), LONG),
        capacity(&b, pool.clone(), LONG),
    );
    assert_eq!(
        left.visit(&first_app).await.unwrap(),
        Visit::Unplaced(zone.clone())
    );
    let delayed = left.reconcile(&zone);
    let newer = async {
        arrived.await.unwrap();
        b.due(&second_app, &zone).await;
        assert_eq!(
            right.visit(&second_app).await.unwrap(),
            Visit::Unplaced(zone.clone())
        );
        assert_eq!(right.reconcile(&zone).await.unwrap(), Exchange::Applied);
        let newer = target(&right, &zone).await;
        assert_eq!(
            (newer.revision, newer.desired, newer.state),
            (2, 2, TargetState::Steady)
        );
        release
            .send(Step::Refuse(Refusal::PoolExhausted))
            .unwrap();
        newer
    };
    let (stale, newer) = futures::join!(delayed, newer);
    assert_eq!(stale.unwrap(), Exchange::Stale);
    assert_eq!(target(&left, &zone).await, newer);
}


/// A failed provider leaves a durable, retryable refusal. The target, the
/// demand and the job survive a manager restart, and the restarted manager
/// sends exactly one request once the retry is due.
async fn restart_target(fixture: &Fixture) {
    let facts = Rc::new(Facts::default());
    let zone = ZoneId::mint();
    let app = AppId::mint();
    {
        let host = Host::new(fixture, facts.clone()).await;
        host.due(&app, &zone).await;
        let pool = Pool::new(Starter::new(host.clone(), 2));
        pool.script.borrow_mut().push_back(Step::Fail);
        let mut driver = host.driver(LONG, pool.clone());
        driver.tick().await;
        assert_eq!(pool.calls(), 1);
        assert_eq!(pool.starts(), 0);
    }
    let host = Host::new(fixture, facts).await;
    let pool = Pool::new(Starter::new(host.clone(), 2));
    let mut driver = host.driver(LONG, pool.clone());
    let failed = target(driver.capacity(), &zone).await;
    assert_eq!(
        (failed.revision, failed.state, failed.refusal),
        (1, TargetState::Refused, Some(Refusal::Unavailable))
    );
    assert_eq!(driver.capacity().demands(&zone).await.unwrap(), vec![app.clone()]);
    let job = rows(fixture, "jobs", value!({"app_id":app.as_str()})).await;
    assert_eq!(job[0]["state"].as_str(), Some("ready"));
    driver.tick().await;
    assert_eq!(pool.calls(), 0, "the refusal still paces the restarted manager");
    retry_now(fixture, &zone).await;
    driver.tick().await;
    assert_eq!(pool.calls(), 1);
    driver.tick().await;
    let worker = pool.started.borrow()[0].1.clone();
    assert_eq!(host.placed(&worker).await, vec![app]);
    assert_eq!(target(driver.capacity(), &zone).await.state, TargetState::Steady);
}

const OWNERLESS: usize = 5;
const SLOTS: u32 = 2;

/// What one race cost: workers started and provider requests.
#[derive(Debug, PartialEq, Eq)]
struct Cost {
    starts: usize,
    calls: usize,
}

/// Owner-less apps, no capacity, two racing replicas, and a provider whose
/// first reply is lost after it acted. The retry happens before the started
/// workers register, as it does while real workers boot.
///
/// The target is a value, not an instruction: it starts no more workers than
/// the unabsorbable placements need at the zone's slots per worker, and the
/// lost reply's retry starts nothing more.
async fn coalesced_starts(fixture: &Fixture) {
    let declarative = Box::pin(race_target(fixture)).await;
    let workers = OWNERLESS.div_ceil(usize::try_from(SLOTS).unwrap());
    assert_eq!(declarative, Cost { starts: workers, calls: 2 });
    assert!(declarative.starts < OWNERLESS);
}

async fn race_target(fixture: &Fixture) -> Cost {
    let (a, b) = replicas(fixture).await;
    let zone = ZoneId::mint();
    let apps: Vec<AppId> = (0..OWNERLESS).map(|_| AppId::mint()).collect();
    for app in &apps {
        a.due(app, &zone).await;
    }
    let pool = Pool::new(Starter::new(a.clone(), SLOTS));
    pool.script.borrow_mut().push_back(Step::Lose);
    let (left, right) = (
        capacity(&a, pool.clone(), LONG),
        capacity(&b, pool.clone(), LONG),
    );
    for app in &apps {
        left.visit(app).await.unwrap();
    }
    for round in 0..2 {
        if round > 0 {
            retry_now(fixture, &zone).await;
        }
        let (x, y) = futures::join!(left.reconcile(&zone), right.reconcile(&zone));
        assert!(matches!(
            (x.unwrap(), y.unwrap()),
            (Exchange::Applied, Exchange::Idle) | (Exchange::Idle, Exchange::Applied)
        ));
    }
    settle(&a, &b, pool.clone(), &apps).await;
    Cost {
        starts: pool.starts(),
        calls: pool.calls(),
    }
}


/// Make an intent's paced retry due without waiting out its interval.

/// Both replicas tick until every app is owned.
async fn settle(a: &Host, b: &Host, provider: Rc<dyn CapacityProvider>, apps: &[AppId]) {
    let mut first = a.driver(LONG, provider.clone());
    let mut second = b.driver(LONG, provider);
    for _ in 0..3 {
        futures::join!(first.tick(), second.tick());
    }
    for app in apps {
        assert!(a.coordinator.owned(app).await.unwrap());
    }
}

/// Replicas blocked on the zone row's lock both proceed once it is released,
/// and still send one request for the one revision.
#[compio::test]
async fn postgres_replicas_waiting_on_the_zone_lock_send_one_request() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    let (a, b) = replicas(&fixture).await;
    let zone = ZoneId::mint();
    let apps: Vec<AppId> = (0..3).map(|_| AppId::mint()).collect();
    for app in &apps {
        a.due(app, &zone).await;
    }
    let pool = Pool::new(Starter::new(a.clone(), 2));
    let (left, right) = (
        capacity(&a, pool.clone(), LONG),
        capacity(&b, pool.clone(), LONG),
    );
    for app in &apps {
        left.visit(app).await.unwrap();
    }
    admin.batch_execute("BEGIN").await.unwrap();
    let locked = admin
        .query(
            "SELECT id FROM workflow_manager.capacity_targets WHERE id=$1 FOR UPDATE",
            &[&zone.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(locked.len(), 1);
    let release = async {
        blocked_manager(admin, 2).await;
        assert_eq!(pool.calls(), 0);
        admin.batch_execute("ROLLBACK").await.unwrap();
    };
    let (x, y, ()) = futures::join!(left.reconcile(&zone), right.reconcile(&zone), release);
    assert!(matches!(
        (x.unwrap(), y.unwrap()),
        (Exchange::Applied, Exchange::Idle) | (Exchange::Idle, Exchange::Applied)
    ));
    let converged = target(&left, &zone).await;
    assert_eq!((converged.revision, converged.desired), (1, 3));
    assert_eq!(pool.calls(), 1);
    assert_eq!(pool.starts(), 2);
}
