//! The retention lane releases queue holds of deployments the app no longer
//! selects, never while their code is needed and never under an activation
//! that is confirming the same hold.
#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native catalog and queue fixtures stay on their compio runtime"
)]

#[allow(dead_code, reason = "shared fixtures expose other manager contracts")]
#[path = "support/retention.rs"]
mod catalog_support;
#[allow(dead_code, reason = "shared fixtures expose other manager contracts")]
mod support;

use catalog_support::{Catalog, Published};
use std::{
    cell::{Cell, RefCell},
    fmt,
    future::Future,
    pin::Pin,
    rc::Rc,
    time::Duration,
};
use support::{Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, RunId, WorkerId},
    workflow_deployments::{HoldGeneration, HoldScope},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, Settlement},
    workflow_schedules::{
        ActivateSchedules, DisableSchedules, RegisterSchedules, ScheduleDescriptor, ScheduleId,
    },
};
use zeroship_data_orm::{
    orm::{Operation, Output},
    value, Value,
};
use zeroship_workflow_calendar::{
    IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleTiming,
};
use zeroship_workflow_manager::{
    lifecycle::{AppLifecycle, Undeletable},
    driver::{Driver, LaneReport, Options},
    recovery::Options as RecoveryOptions,
    retention::{HoldClient, HoldFuture},
    scheduling::{Options as SchedulerOptions, Scheduler},
    Error, Options as QueueOptions, Queue,
};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let fixture = Fixture::new(Backend::Sqlite).await;
            Box::pin($contract(&fixture)).await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = Fixture::new(Backend::Postgres).await;
            Box::pin($contract(&fixture)).await;
        }
    };
}

case!(
    sqlite_superseded_and_archived_deployments_release_their_queue_holds,
    postgres_superseded_and_archived_deployments_release_their_queue_holds,
    release_policy
);
case!(
    sqlite_activation_racing_the_release_pass_commits,
    postgres_activation_racing_the_release_pass_commits,
    racing_activation
);
case!(
    sqlite_release_decides_stale_candidates_under_the_app_lock,
    postgres_release_decides_stale_candidates_under_the_app_lock,
    stale_candidates
);
case!(
    sqlite_hold_without_confirmation_time_is_reported_and_retained,
    postgres_hold_without_confirmation_time_is_reported_and_retained,
    unconfirmed_time
);

/// The default grace, far beyond each contract's duration: a hold is old only
/// once a contract ages it explicitly.
fn undeletable() -> Rc<dyn AppLifecycle> {
    Rc::new(Undeletable)
}

fn options() -> Options {
    Options {
        recovery: RecoveryOptions {
            interval: Duration::from_secs(3600),
            ..RecoveryOptions::default()
        },
        ..Options::default()
    }
}

async fn queue(fixture: &Fixture, holds: Rc<dyn HoldClient>) -> Queue {
    Queue::connect(fixture.binding(), fixture.url(), QueueOptions::default(), holds)
        .await
        .unwrap()
}

fn daily() -> ScheduleDescriptor {
    ScheduleDescriptor {
        name: "daily".into(),
        workflow_name: "scheduled-work".into(),
        schedule: ScheduleTiming::Interval {
            interval_ms: 86_400_000,
            anchor: IntervalAnchor::Epoch,
        },
        overlap: ScheduleOverlap::Allow,
        catch_up: ScheduleCatchUp::Skip,
    }
}

fn activation(app: &AppId, deployment: &Published, revision: i64) -> ActivateSchedules {
    ActivateSchedules {
        app_id: app.clone(),
        deployment_id: deployment.id.clone(),
        revision: revision.try_into().unwrap(),
    }
}

async fn activate(
    scheduler: &Scheduler,
    app: &AppId,
    deployment: &Published,
    revision: i64,
    calendar: Vec<ScheduleDescriptor>,
) -> JobSpec {
    scheduler
        .prepare(&RegisterSchedules {
            app_id: app.clone(),
            deployment_id: deployment.id.clone(),
            schedules: calendar,
        })
        .await
        .unwrap();
    scheduler
        .activate(&activation(app, deployment, revision))
        .await
        .unwrap()
}

fn advance(app: &AppId, deployment: &DeploymentId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Advance {
            deployment_id: deployment.clone(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    }
}

/// Complete every deliverable job, as the app's worker would.
async fn settle_all(queue: &Queue, app: &AppId) {
    let authority = Assignment {
        app_id: app.clone(),
        worker_id: WorkerId::mint(),
        revision: 1.try_into().unwrap(),
        expires_at: i64::MAX.try_into().unwrap(),
    };
    for _ in 0..32 {
        let Some(grant) = queue.claim(&authority).await.unwrap() else {
            return;
        };
        let settlement = Settlement {
            delivery: grant.delivery().clone(),
            outcome: JobOutcome::Completed {},
            successors: vec![],
        };
        queue.settle(&authority, &settlement).await.unwrap();
    }
    panic!("the app queue did not drain");
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
        panic!("expected stored manager rows");
    };
    rows
}

async fn patch(fixture: &Fixture, collection: &str, filter: Value, document: Value) {
    let output = fixture
        .database()
        .await
        .collection(collection)
        .unwrap()
        .execute(Operation::Update {
            filter,
            patch: document,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(output, Output::Count(changed) if changed > 0));
}

/// Move every confirmed hold of the app past any release grace.
async fn age(fixture: &Fixture, app: &AppId) {
    patch(
        fixture,
        "deployment_holds",
        value!({"app_id":app.as_str(),"state":"held"}),
        value!({"held_at":0}),
    )
    .await;
}

async fn intent(fixture: &Fixture, app: &AppId, deployment: &Published) -> Value {
    let mut stored = rows(
        fixture,
        "deployment_holds",
        value!({"app_id":app.as_str(),"deployment_id":deployment.id.as_str()}),
    )
    .await;
    assert_eq!(stored.len(), 1);
    stored.remove(0)
}

async fn hold(fixture: &Fixture, app: &AppId, deployment: &Published) -> (String, i64) {
    let stored = intent(fixture, app, deployment).await;
    (
        stored["state"].as_str().unwrap().to_owned(),
        stored["generation"].as_i64().unwrap(),
    )
}

fn held(generation: i64) -> (String, i64) {
    ("held".into(), generation)
}

fn released(generation: i64) -> (String, i64) {
    ("released".into(), generation)
}

/// Retained holds take no candidate slot: the lane visited nothing.
fn idle(report: &LaneReport) {
    completed(report, 0);
}

fn completed(report: &LaneReport, visited: usize) {
    assert_eq!(report.visited, visited, "{report:?}");
    assert_eq!(report.completed, visited, "{report:?}");
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(report.scan_error, None);
    assert!(!report.timed_out);
}

/// The app's one calendar: its identity, selecting activation and frontier.
async fn calendar(fixture: &Fixture, app: &AppId) -> (String, String, Option<i64>) {
    let stored = rows(fixture, "schedules", value!({"app_id":app.as_str()})).await;
    assert_eq!(stored.len(), 1);
    (
        stored[0]["id"].as_str().unwrap().to_owned(),
        stored[0]["activation_id"].as_str().unwrap().to_owned(),
        stored[0]["next_at"].as_i64(),
    )
}

/// One app moves through replacement, archive and restore.
async fn release_policy(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let queue = queue(fixture, catalog.client()).await;
    assert_eq!(
        Driver::new(
            queue.clone(),
            Options {
                hold_grace: QueueOptions::default().transaction_timeout,
                ..options()
            },
            undeletable(),
        )
        .map(drop),
        Err(Error::Invalid),
        "a grace within an acquirer's budget can release a hold before its dependency commits"
    );
    let mut driver = Driver::new(queue.clone(), options(), undeletable()).unwrap();
    let scheduler = Scheduler::new(queue.clone(), SchedulerOptions::default()).unwrap();
    let app = AppId::mint();
    let first = catalog.publish(&app, "first", &[daily()]).await;
    let second = catalog.publish(&app, "second", &[daily()]).await;
    activate(&scheduler, &app, &first, 1, vec![daily()]).await;
    let (schedule, _, _) = calendar(fixture, &app).await;
    patch(
        fixture,
        "schedules",
        value!({"app_id":app.as_str()}),
        value!({"next_at":0,"anchor_at":0}),
    )
    .await;
    let occurrence = scheduler
        .dispatch(&app, &ScheduleId::parse(&schedule).unwrap())
        .await
        .unwrap();
    assert_eq!(occurrence.jobs.len(), 1);
    age(fixture, &app).await;
    // The enabled selection keeps its hold however old it becomes.
    idle(&driver.tick().await.retention);
    assert_eq!(hold(fixture, &app, &first).await, held(1));

    // Replacement supersedes the first deployment; its accepted jobs still pin it.
    activate(&scheduler, &app, &second, 2, vec![daily()]).await;
    age(fixture, &app).await;
    idle(&driver.tick().await.retention);
    assert_eq!(hold(fixture, &app, &first).await, held(1));
    catalog.assert_retained(&app, &first).await;
    let journal = HoldScope::for_app(app.clone());
    let journal_hold = catalog
        .ledger
        .acquire(&journal, first.id.as_str(), 1.try_into().unwrap())
        .await
        .unwrap();
    settle_all(&queue, &app).await;
    completed(&driver.tick().await.retention, 1);
    assert_eq!(hold(fixture, &app, &first).await, released(1));
    assert_eq!(hold(fixture, &app, &second).await, held(1));
    // Releasing the queue holder leaves the independent journal holder.
    catalog.assert_retained(&app, &first).await;
    catalog
        .ledger
        .release(&journal, first.id.as_str(), journal_hold.generation)
        .await
        .unwrap();
    catalog.reclaim(&app, &first).await;
    catalog.assert_retained(&app, &second).await;
    idle(&driver.tick().await.retention);

    // Archive disables the calendar; its frozen frontier needs no code.
    let (_, selected, frontier) = calendar(fixture, &app).await;
    assert!(frontier.is_some());
    scheduler
        .disable(&DisableSchedules {
            app_id: app.clone(),
            revision: 3.try_into().unwrap(),
        })
        .await
        .unwrap();
    settle_all(&queue, &app).await;
    age(fixture, &app).await;
    completed(&driver.tick().await.retention, 1);
    assert_eq!(hold(fixture, &app, &second).await, released(1));
    assert_eq!(
        calendar(fixture, &app).await,
        (schedule.clone(), selected, frontier)
    );

    // Restore publishes a fresh activation, which holds the code again before
    // the retained calendar resumes.
    let restored = scheduler
        .activate(&activation(&app, &second, 4))
        .await
        .unwrap();
    assert_eq!(hold(fixture, &app, &second).await, held(2));
    assert_eq!(
        calendar(fixture, &app).await,
        (schedule, restored.id.as_str().to_owned(), frontier)
    );
    age(fixture, &app).await;
    idle(&driver.tick().await.retention);
    assert_eq!(hold(fixture, &app, &second).await, held(2));
    catalog.assert_retained(&app, &second).await;
}

/// Runs another manager replica's driver after Control confirms each hold this
/// queue acquires, before the acquirer's own transaction commits its dependency.
#[derive(Debug)]
struct RacingHolds {
    inner: Rc<dyn HoldClient>,
    replica: RefCell<Option<Driver>>,
    armed: Cell<bool>,
    races: Cell<usize>,
}

impl HoldClient for RacingHolds {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move {
            let receipt = self.inner.acquire(app, deployment, generation).await?;
            if self.armed.get() {
                let mut replica = self.replica.borrow_mut().take().unwrap();
                // The first pass resumes the confirmed intent; later ones see
                // a held deployment that nothing selects or uses yet.
                for _ in 0..3 {
                    replica.tick().await;
                }
                *self.replica.borrow_mut() = Some(replica);
                self.races.set(self.races.get() + 1);
            }
            Ok(receipt)
        })
    }

    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        self.inner.release(app, deployment, generation)
    }
}

async fn racing_activation(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let replica = Driver::new(queue(fixture, catalog.client()).await, options(), undeletable()).unwrap();
    let racing = Rc::new(RacingHolds {
        inner: catalog.client(),
        replica: RefCell::new(Some(replica)),
        armed: Cell::new(false),
        races: Cell::new(0),
    });
    let queue = queue(fixture, racing.clone()).await;
    let scheduler = Scheduler::new(queue.clone(), SchedulerOptions::default()).unwrap();
    let mut driver = Driver::new(queue.clone(), options(), undeletable()).unwrap();
    let app = AppId::mint();
    let first = catalog.publish(&app, "first", &[]).await;
    let second = catalog.publish(&app, "second", &[]).await;
    activate(&scheduler, &app, &first, 1, vec![]).await;
    activate(&scheduler, &app, &second, 2, vec![]).await;
    settle_all(&queue, &app).await;
    age(fixture, &app).await;
    completed(&driver.tick().await.retention, 1);
    assert_eq!(hold(fixture, &app, &first).await, released(1));

    // Roll back to the first deployment. Its activation reacquires the hold
    // outside the queue transaction while another replica's lane passes.
    racing.armed.set(true);
    let mut refused = 0;
    let job = loop {
        match scheduler.activate(&activation(&app, &first, 3)).await {
            Ok(job) => break job,
            // Control's publisher resends the same revision after a refusal.
            Err(Error::Conflict) if refused < 3 => refused += 1,
            Err(error) => panic!("the racing activation never committed: {error:?}"),
        }
    };
    racing.armed.set(false);
    assert_eq!(
        refused, 0,
        "the release pass took the hold the activation confirmed"
    );
    assert!(racing.races.get() > 0);
    assert!(matches!(
        &job.operation,
        JobOperation::Activate { deployment_id, .. } if deployment_id == &first.id
    ));
    assert_eq!(hold(fixture, &app, &first).await, held(2));
    assert_eq!(
        scheduler
            .selection(&app)
            .await
            .unwrap()
            .unwrap()
            .activation
            .unwrap()
            .deployment_id,
        first.id
    );
    // The replaced deployment is released once old; the new selection is not.
    age(fixture, &app).await;
    completed(&driver.tick().await.retention, 1);
    assert_eq!(hold(fixture, &app, &second).await, released(1));
    assert_eq!(hold(fixture, &app, &first).await, held(2));
}

type Action = Pin<Box<dyn Future<Output = ()>>>;

/// Runs one action inside the first release Control receives, after the lane
/// has fetched its candidate page and before it decides the later candidates.
struct ReleaseHook {
    inner: Rc<dyn HoldClient>,
    action: RefCell<Option<Action>>,
}

impl fmt::Debug for ReleaseHook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReleaseHook")
            .field("armed", &self.action.borrow().is_some())
            .finish_non_exhaustive()
    }
}

impl HoldClient for ReleaseHook {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        self.inner.acquire(app, deployment, generation)
    }

    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move {
            let receipt = self.inner.release(app, deployment, generation).await?;
            let action = self.action.borrow_mut().take();
            if let Some(action) = action {
                action.await;
            }
            Ok(receipt)
        })
    }
}

async fn stale_candidates(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let hook = Rc::new(ReleaseHook {
        inner: catalog.client(),
        action: RefCell::new(None),
    });
    let queue = queue(fixture, hook.clone()).await;
    let peer = self::queue(fixture, catalog.client()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let mut published = Vec::new();
    for marker in ["one", "two", "three", "four"] {
        let deployment = catalog.publish(&app, marker, &[]).await;
        queue.ensure_deployment(&app, &deployment.id).await.unwrap();
        let id = intent(fixture, &app, &deployment).await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        published.push((id, deployment));
    }
    // The lane visits candidates in hold identity order.
    published.sort_by(|a, b| a.0.cmp(&b.0));
    let mut published = published.into_iter().map(|(_, deployment)| deployment);
    let mut next = || published.next().unwrap();
    let (first, reacquired, selected, pinned) = (next(), next(), next(), next());
    age(fixture, &app).await;
    let work = advance(&app, &pinned.id);
    let action = {
        let (peer, app, work) = (peer.clone(), app.clone(), work.clone());
        let (reacquired, selected) = (reacquired.id.clone(), selected.id.clone());
        async move {
            // Reacquisition restarts the grace of a hold already on the page.
            peer.release_deployment(&app, &reacquired).await.unwrap();
            peer.ensure_deployment(&app, &reacquired).await.unwrap();
            // A deployment on the page becomes the enabled selection, and its
            // settled activation leaves no job to retain it.
            let scheduler = Scheduler::new(peer.clone(), SchedulerOptions::default()).unwrap();
            scheduler
                .prepare(&RegisterSchedules {
                    app_id: app.clone(),
                    deployment_id: selected.clone(),
                    schedules: vec![],
                })
                .await
                .unwrap();
            scheduler
                .activate(&ActivateSchedules {
                    app_id: app.clone(),
                    deployment_id: selected,
                    revision: 1.try_into().unwrap(),
                })
                .await
                .unwrap();
            settle_all(&peer, &app).await;
            // New work pins another deployment on the page.
            peer.submit(&work).await.unwrap();
        }
    };
    *hook.action.borrow_mut() = Some(Box::pin(action));
    let mut driver = Driver::new(queue.clone(), options(), undeletable()).unwrap();
    completed(&driver.tick().await.retention, 4);
    assert!(hook.action.borrow().is_none(), "the page outlived the hook");
    assert_eq!(hold(fixture, &app, &first).await, released(1));
    assert_eq!(hold(fixture, &app, &reacquired).await, held(2));
    assert_eq!(hold(fixture, &app, &selected).await, held(1));
    assert_eq!(hold(fixture, &app, &pinned).await, held(1));
    catalog.assert_retained(&app, &pinned).await;

    // A later pass releases what stayed in use, once nothing needs it.
    settle_all(&queue, &app).await;
    age(fixture, &app).await;
    completed(&driver.tick().await.retention, 2);
    assert_eq!(hold(fixture, &app, &reacquired).await, released(2));
    assert_eq!(hold(fixture, &app, &pinned).await, released(1));
    assert_eq!(hold(fixture, &app, &selected).await, held(1));
    catalog.reclaim(&app, &pinned).await;
}

async fn unconfirmed_time(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let queue = queue(fixture, catalog.client()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let deployment = catalog.publish(&app, "damaged", &[]).await;
    queue.ensure_deployment(&app, &deployment.id).await.unwrap();
    let id = intent(fixture, &app, &deployment).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A confirmed hold always records its time; without one its age is unknown.
    patch(
        fixture,
        "deployment_holds",
        value!({"id":id.as_str()}),
        value!({"held_at":null}),
    )
    .await;
    let mut driver = Driver::new(queue.clone(), options(), undeletable()).unwrap();
    for _ in 0..2 {
        let report = driver.tick().await.retention;
        assert_eq!(report.visited, 1);
        assert_eq!(report.completed, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].id, id);
        assert_eq!(report.failures[0].error, Error::Storage);
        assert_eq!(hold(fixture, &app, &deployment).await, held(1));
        catalog.assert_retained(&app, &deployment).await;
    }
}
