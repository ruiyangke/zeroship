//! The capacity contracts head to head: a declarative per-zone target against
//! per-app provisioning intents, on one eligibility predicate.
#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native capacity fixtures stay on their compio runtime"
)]

#[allow(dead_code, reason = "shared fixtures expose other manager contracts")]
mod support;
#[allow(dead_code, reason = "shared placement fixtures serve two contract suites")]
#[path = "support/placement.rs"]
mod placement_support;

use futures::{channel::oneshot, future::ready};
use placement_support::{
    blocked_manager, options, revision, Counted, Facts, Host, Pool, Starter, Starts, Step, LONG,
    SOON,
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
        self, Capacity, Contract, Exchange, IntentState, Refusal, StaticPool, TargetState, Visit,
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
    sqlite_a_stale_reply_cannot_overwrite_a_newer_intent,
    postgres_a_stale_reply_cannot_overwrite_a_newer_intent,
    stale_intent
);
case!(
    sqlite_targets_demand_and_jobs_survive_provider_failure_and_restart,
    postgres_targets_demand_and_jobs_survive_provider_failure_and_restart,
    restart_target
);
case!(
    sqlite_intents_and_jobs_survive_provider_failure_and_restart,
    postgres_intents_and_jobs_survive_provider_failure_and_restart,
    restart_intent
);
case!(
    sqlite_racing_replicas_start_no_more_workers_than_unabsorbable_placements,
    postgres_racing_replicas_start_no_more_workers_than_unabsorbable_placements,
    head_to_head
);

fn declarative(pool: &Rc<Pool>) -> Contract {
    Contract::declarative(pool.clone())
}

fn capacity(host: &Host, contract: Contract, retry: Duration) -> Capacity {
    Capacity::new(host.coordinator.clone(), contract, options(retry).capacity).unwrap()
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
        let contract = if started {
            declarative(&pool)
        } else {
            Contract::declarative(Rc::new(StaticPool))
        };
        let mut driver = host.driver(LONG, contract);
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
    let mut first = a.driver(LONG, Contract::declarative(pool.clone()));
    let mut second = b.driver(LONG, Contract::declarative(pool.clone()));
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
        capacity(&a, declarative(&pool), LONG),
        capacity(&b, declarative(&pool), LONG),
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

    let mut first = a.driver(LONG, declarative(&pool));
    let mut second = b.driver(LONG, declarative(&pool));
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
        Contract::declarative(pool.clone()),
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
        declarative(&pool),
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
        capacity(&a, declarative(&pool), LONG),
        capacity(&b, declarative(&pool), LONG),
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

/// The comparison contract fences the same way, by generation.
async fn stale_intent(fixture: &Fixture) {
    let (a, b) = replicas(fixture).await;
    let zone = ZoneId::mint();
    let app = AppId::mint();
    a.due(&app, &zone).await;
    let starts = Starts::new(Starter::new(a.clone(), 1), true);
    let (entered, arrived) = oneshot::channel();
    let (release, released) = oneshot::channel();
    starts.script.borrow_mut().push_back(Step::Gate {
        entered,
        release: released,
    });
    let (left, right) = (
        capacity(&a, Contract::intents(starts.clone()), LONG),
        capacity(&b, Contract::intents(starts.clone()), LONG),
    );
    assert_eq!(left.visit(&app).await.unwrap(), Visit::Unplaced(zone.clone()));
    let delayed = left.request(&app);
    let newer = async {
        arrived.await.unwrap();
        // The app is placed, settling generation one, then needs an owner again.
        let worker = b.worker(&zone, 1).await;
        assert!(matches!(right.visit(&app).await.unwrap(), Visit::Placed(_)));
        assert_eq!(
            right.intent(&app).await.unwrap().unwrap().state,
            IntentState::Settled
        );
        b.facts.revoke(&worker);
        assert_eq!(right.visit(&app).await.unwrap(), Visit::Unplaced(zone.clone()));
        assert_eq!(right.request(&app).await.unwrap(), Exchange::Applied);
        let newer = right.intent(&app).await.unwrap().unwrap();
        assert_eq!(
            (newer.generation, newer.state),
            (2, IntentState::Provisioned)
        );
        release
            .send(Step::Refuse(Refusal::PoolExhausted))
            .unwrap();
        newer
    };
    let (stale, newer) = futures::join!(delayed, newer);
    assert_eq!(stale.unwrap(), Exchange::Stale);
    assert_eq!(left.intent(&app).await.unwrap().unwrap(), newer);
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
        let mut driver = host.driver(LONG, declarative(&pool));
        driver.tick().await;
        assert_eq!(pool.calls(), 1);
        assert_eq!(pool.starts(), 0);
    }
    let host = Host::new(fixture, facts).await;
    let pool = Pool::new(Starter::new(host.clone(), 2));
    let mut driver = host.driver(LONG, declarative(&pool));
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

/// The comparison contract's intent, demand and job survive the same way.
async fn restart_intent(fixture: &Fixture) {
    let facts = Rc::new(Facts::default());
    let zone = ZoneId::mint();
    let app = AppId::mint();
    {
        let host = Host::new(fixture, facts.clone()).await;
        host.due(&app, &zone).await;
        let starts = Starts::new(Starter::new(host.clone(), 1), true);
        starts.script.borrow_mut().push_back(Step::Fail);
        let mut driver = host.driver(LONG, Contract::intents(starts.clone()));
        driver.tick().await;
        assert_eq!(starts.calls(), 1);
        assert_eq!(starts.starts(), 0);
    }
    let host = Host::new(fixture, facts).await;
    let starts = Starts::new(Starter::new(host.clone(), 1), true);
    let mut driver = host.driver(SOON, Contract::intents(starts.clone()));
    let failed = driver.capacity().intent(&app).await.unwrap().unwrap();
    assert_eq!(
        (failed.generation, failed.state, failed.refusal),
        (1, IntentState::Refused, Some(Refusal::Unavailable))
    );
    let job = rows(fixture, "jobs", value!({"app_id":app.as_str()})).await;
    assert_eq!(job[0]["state"].as_str(), Some("ready"));
    // The previous manager set a long retry; make it due for this one.
    retry_intent_now(fixture, &app).await;
    driver.tick().await;
    assert_eq!(starts.calls(), 1);
    driver.tick().await;
    let worker = starts.started.borrow()[0].2.clone();
    assert_eq!(host.placed(&worker).await, vec![app.clone()]);
    assert_eq!(
        driver.capacity().intent(&app).await.unwrap().unwrap().state,
        IntentState::Settled
    );
}

const OWNERLESS: usize = 5;
const SLOTS: u32 = 2;

/// What one contract's race cost: workers started and provider requests.
#[derive(Debug, PartialEq, Eq)]
struct Cost {
    starts: usize,
    calls: usize,
}

/// Five owner-less apps, no capacity, two racing replicas, and a provider
/// whose first reply is lost after it acted. The retry happens before the
/// started workers register, as it does while real workers boot.
///
/// A declarative target never starts more workers than the unabsorbable
/// placements need, and a lost reply's retry starts nothing. Per-app intents
/// start one worker per app, and without provider-side deduplication the
/// retried intent starts another: more starts than unabsorbable placements.
async fn head_to_head(fixture: &Fixture) {
    let declarative = Box::pin(race_target(fixture)).await;
    let deduplicated = Box::pin(race_intents(fixture, true)).await;
    let blind = Box::pin(race_intents(fixture, false)).await;
    eprintln!(
        "capacity head to head: {OWNERLESS} owner-less apps, {SLOTS} slots per worker; \
         target {declarative:?}; intents with dedupe {deduplicated:?}; \
         intents without dedupe {blind:?}"
    );
    let workers = OWNERLESS.div_ceil(usize::try_from(SLOTS).unwrap());
    assert_eq!(declarative, Cost { starts: workers, calls: 2 });
    assert!(declarative.starts <= OWNERLESS);
    assert_eq!(
        deduplicated,
        Cost {
            starts: OWNERLESS,
            calls: OWNERLESS + 1
        }
    );
    assert_eq!(
        blind,
        Cost {
            starts: OWNERLESS + 1,
            calls: OWNERLESS + 1
        }
    );
    assert!(
        blind.starts > OWNERLESS,
        "per-app intents need provider-side deduplication"
    );
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
        capacity(&a, declarative(&pool), LONG),
        capacity(&b, declarative(&pool), LONG),
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
    settle(&a, &b, declarative(&pool), &apps).await;
    Cost {
        starts: pool.starts(),
        calls: pool.calls(),
    }
}

async fn race_intents(fixture: &Fixture, dedupe: bool) -> Cost {
    let (a, b) = replicas(fixture).await;
    let zone = ZoneId::mint();
    let apps: Vec<AppId> = (0..OWNERLESS).map(|_| AppId::mint()).collect();
    for app in &apps {
        a.due(app, &zone).await;
    }
    let starts = Starts::new(Starter::new(a.clone(), SLOTS), dedupe);
    starts.script.borrow_mut().push_back(Step::Lose);
    let (left, right) = (
        capacity(&a, Contract::intents(starts.clone()), LONG),
        capacity(&b, Contract::intents(starts.clone()), LONG),
    );
    for app in &apps {
        left.visit(app).await.unwrap();
    }
    for round in 0..2 {
        if round > 0 {
            for app in &apps {
                retry_intent_now(fixture, app).await;
            }
        }
        for app in &apps {
            let (x, y) = futures::join!(left.request(app), right.request(app));
            assert!(matches!(
                (x.unwrap(), y.unwrap()),
                (Exchange::Applied | Exchange::Idle, Exchange::Idle)
                    | (Exchange::Idle, Exchange::Applied)
            ));
        }
    }
    settle(&a, &b, Contract::intents(starts.clone()), &apps).await;
    Cost {
        starts: starts.starts(),
        calls: starts.calls(),
    }
}

/// Make an intent's paced retry due without waiting out its interval.
async fn retry_intent_now(fixture: &Fixture, app: &AppId) {
    let updated = fixture
        .database()
        .await
        .collection("capacity_intents")
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":app.as_str()}),
            patch: value!({"retry_at":0}),
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(updated, Output::Count(1)), "{updated:?}");
}

/// Both replicas tick until every app is owned.
async fn settle(a: &Host, b: &Host, contract: Contract, apps: &[AppId]) {
    let mut first = a.driver(LONG, contract.clone());
    let mut second = b.driver(LONG, contract);
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
        capacity(&a, declarative(&pool), LONG),
        capacity(&b, declarative(&pool), LONG),
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
