//! The closing lane: idle and archived triggers, attempt timeout and backoff,
//! and abandonment of responsibility for apps Control deleted.
#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native closing fixtures stay on their compio runtime"
)]

#[allow(
    dead_code,
    reason = "shared queue fixtures also expose backend administration"
)]
mod support;

use futures::future::ready;
use std::{
    cell::RefCell,
    collections::BTreeSet,
    future::Future,
    num::NonZeroU32,
    pin::Pin,
    rc::Rc,
    time::{Duration, Instant},
};
use support::{Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, RegisterWorker, Revision, WorkerId, WorkerState,
    },
    workflow_jobs::{
        BroadcastId, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, Settlement, SubmitJob,
    },
    workflow_policy::{AppPolicy, EstablishIngress, PolicyLeaseRequest},
    workflow_schedules::DisableSchedules,
};
use zeroship_data_orm::{
    orm::{Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    coordinator::{self, Coordinator, Placed},
    driver,
    lifecycle::AppLifecycle,
    policy::{PolicyObservation, PolicySource},
    recovery::{self, Closing, DutyKind, Recovery, Responsibility, ScopeState},
    scheduling::{self, Scheduler},
    DeliveryGrant, Error, Queue,
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
    sqlite_only_idle_scopes_begin_closing,
    postgres_only_idle_scopes_begin_closing,
    idleness
);
case!(
    sqlite_archived_scopes_begin_closing_while_active,
    postgres_archived_scopes_begin_closing_while_active,
    archived
);
case!(
    sqlite_attempts_expire_and_back_off_per_scope,
    postgres_attempts_expire_and_back_off_per_scope,
    pacing
);
case!(
    sqlite_live_work_defers_an_attempt_with_backoff,
    postgres_live_work_defers_an_attempt_with_backoff,
    deferred
);
case!(
    sqlite_deleted_apps_are_abandoned_and_never_reopen,
    postgres_deleted_apps_are_abandoned_and_never_reopen,
    abandonment
);
case!(
    sqlite_the_driver_closes_idle_scopes_and_abandons_deleted_apps,
    postgres_the_driver_closes_idle_scopes_and_abandons_deleted_apps,
    driver_lane
);

const KEY: &str = "verified-enrolled-key";
const HOUR: Duration = Duration::from_secs(3600);
const TICK: Duration = Duration::from_millis(1);

fn revision(value: i64) -> Revision {
    value.try_into().unwrap()
}

/// Bounds a test chooses per call, so no case waits on a real idle window.
const fn options(
    idle: Duration,
    timeout: Duration,
    backoff: Duration,
    max: Duration,
) -> recovery::Options {
    recovery::Options {
        interval: HOUR,
        page_size: 16,
        idle_after: idle,
        closing_timeout: timeout,
        closing_backoff: backoff,
        closing_backoff_max: max,
    }
}

#[derive(Debug)]
struct Source(PolicyObservation);

impl PolicySource for Source {
    fn observe<'a>(
        &'a self,
        _: &'a AppId,
    ) -> Pin<Box<dyn Future<Output = Result<PolicyObservation, Error>> + 'a>> {
        Box::pin(ready(Ok(self.0.clone())))
    }

    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, Error> {
        Ok(observation.expires_at())
    }
}

struct Host {
    queue: Queue,
    coordinator: Coordinator,
    app: AppId,
    worker: WorkerId,
    scope: AssignedScope,
}

/// An activated scope at ingress epoch one, placed on one ready worker.
async fn host(fixture: &Fixture) -> Host {
    let queue = Queue::connect(
        fixture.binding(),
        fixture.url(),
        zeroship_workflow_manager::Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    let coordinator = support::coordinator(&queue, coordinator::Options::default());
    let app = AppId::mint();
    let worker = WorkerId::mint();
    coordinator
        .register(
            &worker,
            &RegisterWorker {
                capacity: NonZeroU32::new(4).unwrap(),
                state: WorkerState::Ready,
            },
        )
        .await
        .unwrap();
    let Placed::Assigned(assignment) = coordinator.place(&app).await.unwrap() else {
        panic!("the app has one eligible worker");
    };
    let host = Host {
        queue,
        coordinator,
        app,
        worker,
        scope: AssignedScope {
            app_id: assignment.app_id,
            assignment_revision: assignment.revision,
        },
    };
    host.recovery(options(HOUR, HOUR, HOUR, HOUR))
        .ensure(&host.app, &DeploymentId::mint(), revision(1))
        .await
        .unwrap();
    host
}

impl Host {
    fn recovery(&self, options: recovery::Options) -> Recovery {
        Recovery::new(self.queue.clone(), options).unwrap()
    }

    async fn state(&self) -> Responsibility {
        self.recovery(options(HOUR, HOUR, HOUR, HOUR))
            .responsibility(&self.app)
            .await
            .unwrap()
            .expect("activated scope")
    }

    async fn claim(&self) -> Option<DeliveryGrant> {
        self.coordinator
            .claim_job(&self.worker, &self.scope, support::delivery_ceiling(), || ready(Ok(self.worker.clone())))
            .await
            .unwrap()
    }

    async fn settle(&self, grant: &DeliveryGrant, outcome: JobOutcome) {
        self.coordinator
            .settle_job(
                &self.worker,
                &Settlement {
                    delivery: grant.delivery().clone(),
                    outcome,
                    successors: Vec::new(),
                },
                || ready(Ok(self.worker.clone())),
            )
            .await
            .unwrap();
    }

    /// Deliver the attempt's Close and settle it with `drained` evidence.
    async fn settle_close(&self, close: &JobSpec, drained: bool) {
        let grant = self.claim().await.expect("closure is delivered");
        assert_eq!(&grant.delivery().job, close);
        self.settle(&grant, JobOutcome::Closed { drained }).await;
    }

    async fn publish(&self, job: &JobSpec) -> Result<JobSpec, Error> {
        self.coordinator
            .submit_job(
                &self.worker,
                &SubmitJob {
                    scope: self.scope.clone(),
                    job: job.clone(),
                },
                || ready(Ok(self.worker.clone())),
            )
            .await
    }

    async fn lease(&self, establish: Option<EstablishIngress>) -> Result<Option<Revision>, Error> {
        let source = Source(
            PolicyObservation::new(
                self.app.clone(),
                revision(1),
                AppPolicy::default(),
                Instant::now() + HOUR,
            )
            .unwrap(),
        );
        let request = PolicyLeaseRequest {
            scope: self.scope.clone(),
            establish,
            ingress_used: false,
        };
        self.coordinator
            .policy_lease(&self.worker, KEY, &request, &source, || {
                ready(Ok(self.worker.clone()))
            })
            .await
            .map(|grant| grant.ingress_epoch())
    }
}

fn fanout(app: &AppId, available_at: i64) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Fanout {
            broadcast_id: BroadcastId::mint(),
            revision: revision(1),
        },
        available_at: available_at.try_into().unwrap(),
    }
}

const FUTURE: i64 = i64::MAX / 2;

async fn rows(fixture: &Fixture, collection: &str, filter: Value) -> Vec<Value> {
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection(collection)
        .unwrap()
        .find(filter, value!({"limit":64}))
        .await
        .unwrap()
    else {
        panic!("expected rows");
    };
    rows
}

/// Keep the duties from falling due, so no maintenance job blocks closing.
async fn postpone_duties(fixture: &Fixture, app: &AppId) {
    let updated = fixture
        .database()
        .await
        .collection("recovery_duties")
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"app_id":app.as_str()}),
            patch: value!({"next_due_at":FUTURE}),
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(updated, Output::Count(2)), "{updated:?}");
}

/// Rewind scope columns the manager stamps from its clock, standing in for
/// elapsed time a test would otherwise have to wait out.
async fn rewind(fixture: &Fixture, app: &AppId, patch: Value) {
    let updated = fixture
        .database()
        .await
        .collection("recovery_scopes")
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":app.as_str()}),
            patch,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(updated, Output::Count(1)), "{updated:?}");
}

async fn duties(fixture: &Fixture, app: &AppId) -> usize {
    rows(fixture, "recovery_duties", value!({"app_id":app.as_str()}))
        .await
        .len()
}

/// Manager time at which the attempt's Close job was published.
async fn begun_at(fixture: &Fixture, close: &JobSpec) -> i64 {
    let jobs = rows(fixture, "jobs", value!({"id":close.id.as_str()})).await;
    jobs[0]["created_at"].as_i64().unwrap()
}

fn started(turn: Closing) -> JobSpec {
    let Closing::Started(close) = turn else {
        panic!("expected a new closing attempt, got {turn:?}");
    };
    close
}

/// The idle trigger needs `idle_after` without activity; the same scope stays
/// open under a longer window. A retired scope has nothing to close.
async fn idleness(fixture: &Fixture) {
    let host = host(fixture).await;
    let patient = host.recovery(options(HOUR, HOUR, HOUR, HOUR));
    assert_eq!(
        patient.closing_turn(&host.app).await.unwrap(),
        Closing::Kept
    );
    assert_eq!(host.state().await.state, ScopeState::Open);
    // Reported ingress and a worker publication each restart the idle window.
    let watchful = host.recovery(options(TICK * 5000, HOUR, HOUR, HOUR));
    rewind(fixture, &host.app, value!({"active_at":0})).await;
    watchful.note_ingress(&host.app).await.unwrap();
    assert_eq!(
        watchful.closing_turn(&host.app).await.unwrap(),
        Closing::Kept,
        "reported ingress restarts the idle window"
    );
    rewind(fixture, &host.app, value!({"active_at":0})).await;
    host.publish(&fanout(&host.app, FUTURE)).await.unwrap();
    assert_eq!(
        watchful.closing_turn(&host.app).await.unwrap(),
        Closing::Kept,
        "a worker publication restarts the idle window"
    );
    // The same turn over the same aged row, with no activity, begins closing.
    rewind(fixture, &host.app, value!({"active_at":0})).await;
    let close = started(watchful.closing_turn(&host.app).await.unwrap());
    assert_eq!(close.operation, JobOperation::Close { epoch: revision(1) });
    assert_eq!(
        watchful.closing_turn(&host.app).await.unwrap(),
        Closing::Pending(close.clone()),
        "replicas and later turns converge on one attempt"
    );
    assert_eq!(host.state().await.state, ScopeState::Closing);
    host.settle_close(&close, true).await;
    assert_eq!(host.state().await.state, ScopeState::Retired);
    assert_eq!(
        watchful.closing_turn(&host.app).await.unwrap(),
        Closing::Inactive
    );
    let retired = host.state().await;
    assert!(retired.close_after.is_none() && retired.close_attempts == 0);
}

/// Archive disables the calendar; the trigger closes the scope although it is
/// active, and a later restore that reopens it resets the pacing.
async fn archived(fixture: &Fixture) {
    let host = host(fixture).await;
    let patient = host.recovery(options(HOUR, HOUR, HOUR, HOUR));
    assert_eq!(
        patient.closing_turn(&host.app).await.unwrap(),
        Closing::Kept
    );
    Scheduler::new(host.queue.clone(), scheduling::Options::default())
        .unwrap()
        .disable(&DisableSchedules {
            app_id: host.app.clone(),
            revision: revision(2),
        })
        .await
        .unwrap();
    let close = started(patient.closing_turn(&host.app).await.unwrap());
    host.settle_close(&close, true).await;
    assert_eq!(host.state().await.state, ScopeState::Retired);
}

/// An attempt past its timeout returns the scope to open, and every attempt
/// that does not retire delays the next by its timeout plus a backoff that
/// doubles to its ceiling. Reopening resets the pacing.
async fn pacing(fixture: &Fixture) {
    let host = host(fixture).await;
    compio::time::sleep(TICK * 5).await;
    // Expiry, then a long backoff that defers the next attempt.
    let slow = host.recovery(options(TICK, TICK, HOUR, HOUR));
    let expired = started(slow.closing_turn(&host.app).await.unwrap());
    compio::time::sleep(TICK * 5).await;
    assert_eq!(
        slow.closing_turn(&host.app).await.unwrap(),
        Closing::Expired
    );
    let reopened = host.state().await;
    assert_eq!(reopened.state, ScopeState::Open);
    assert_eq!(reopened.ingress_epoch, revision(1));
    assert_eq!(reopened.close_attempts, 1);
    assert_eq!(
        slow.closing_turn(&host.app).await.unwrap(),
        Closing::Deferred
    );
    // The expired Close still settles, without touching the reopened scope.
    host.settle_close(&expired, true).await;
    assert_eq!(host.state().await, reopened);

    // Establishment reopens at the next epoch and clears the pacing.
    assert_eq!(
        host.lease(Some(EstablishIngress {
            after: Some(revision(1))
        }))
        .await,
        Ok(Some(revision(2)))
    );
    let fresh = host.state().await;
    assert_eq!(fresh.close_attempts, 0);
    assert!(fresh.close_after.is_none());

    // A quick backoff: undrained attempts double their delay to the ceiling.
    let quick = host.recovery(options(TICK, TICK * 20, TICK * 20, TICK * 80));
    let mut delays = Vec::new();
    for attempt in 1..=4 {
        let close = loop {
            match quick.closing_turn(&host.app).await.unwrap() {
                Closing::Started(close) => break close,
                Closing::Deferred | Closing::Kept => compio::time::sleep(TICK * 10).await,
                other => panic!("attempt {attempt}: {other:?}"),
            }
        };
        let state = host.state().await;
        assert_eq!(state.close_attempts, attempt);
        delays.push(state.close_after.unwrap() - begun_at(fixture, &close).await);
        host.settle_close(&close, false).await;
        assert_eq!(host.state().await.state, ScopeState::Open);
    }
    assert_eq!(delays, [20 + 20, 20 + 40, 20 + 80, 20 + 80]);
}

/// A live lease refuses an attempt without publishing Close; the refusal
/// counts as an attempt and backs off before the next one.
async fn deferred(fixture: &Fixture) {
    let host = host(fixture).await;
    host.publish(&fanout(&host.app, 0)).await.unwrap();
    let leased = host.claim().await.unwrap();
    let quick = host.recovery(options(TICK, HOUR, HOUR, HOUR));
    rewind(fixture, &host.app, value!({"active_at":0})).await;
    assert_eq!(
        quick.closing_turn(&host.app).await.unwrap(),
        Closing::Deferred
    );
    let refused = host.state().await;
    assert_eq!(refused.state, ScopeState::Open);
    assert_eq!(refused.close_attempts, 1);
    assert!(refused.close_job.is_none());
    assert!(rows(
        fixture,
        "jobs",
        value!({"app_id":host.app.as_str(),"operation_kind":"close"})
    )
    .await
    .is_empty());
    assert_eq!(
        quick.closing_turn(&host.app).await.unwrap(),
        Closing::Deferred,
        "the backoff defers the next turn"
    );
    assert_eq!(
        host.state().await.close_attempts,
        1,
        "a turn the backoff defers makes no attempt"
    );
    host.settle(&leased, JobOutcome::Completed {}).await;
    rewind(fixture, &host.app, value!({"active_at":0,"close_after":0})).await;
    let close = started(quick.closing_turn(&host.app).await.unwrap());
    assert_eq!(host.state().await.close_attempts, 2);
    host.settle_close(&close, true).await;
    assert_eq!(host.state().await.state, ScopeState::Retired);
}

/// Abandonment deletes the duties and cancels an attempt. Nothing reopens an
/// abandoned scope: not establishment, activation, a worker publication or an
/// intent-producing claim, and its cancelled Close settles without effect.
async fn abandonment(fixture: &Fixture) {
    let host = host(fixture).await;
    let recovery = host.recovery(options(TICK, HOUR, HOUR, HOUR));
    let timer = fanout(&host.app, 0);
    host.publish(&timer).await.unwrap();
    let close = recovery.begin_close(&host.app).await.unwrap().unwrap();
    assert!(recovery.abandon(&host.app).await.unwrap());
    assert!(
        !recovery.abandon(&host.app).await.unwrap(),
        "abandonment is idempotent"
    );
    let abandoned = host.state().await;
    assert_eq!(abandoned.state, ScopeState::Abandoned);
    assert_eq!(abandoned.ingress_epoch, revision(1));
    assert!(abandoned.close_job.is_none() && abandoned.closing_watermark.is_none());
    assert_eq!(duties(fixture, &host.app).await, 0);

    assert_eq!(
        host.lease(Some(EstablishIngress { after: None })).await,
        Err(Error::Denied)
    );
    // Abandonment answers before the epoch bound is judged, so a host naming an
    // epoch above the last one is refused rather than told to retry lower.
    assert_eq!(
        host.lease(Some(EstablishIngress {
            after: Some(revision(2))
        }))
        .await,
        Err(Error::Denied)
    );
    assert_eq!(host.lease(None).await, Ok(None));
    assert_eq!(
        recovery.establish(&host.app, None, true).await,
        Err(Error::Denied)
    );
    assert_eq!(
        recovery
            .ensure(&host.app, &DeploymentId::mint(), revision(2))
            .await,
        Err(Error::Denied)
    );
    host.publish(&fanout(&host.app, FUTURE)).await.unwrap();
    recovery.note_ingress(&host.app).await.unwrap();
    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, timer);
    host.settle(&grant, JobOutcome::Completed {}).await;
    host.settle_close(&close, true).await;
    assert_eq!(host.state().await, abandoned);
    assert_eq!(duties(fixture, &host.app).await, 0);
    assert_eq!(
        recovery.closing_turn(&host.app).await.unwrap(),
        Closing::Inactive
    );
    assert!(recovery.begin_close(&host.app).await.unwrap().is_none());
    for kind in [DutyKind::Reconcile, DutyKind::Collect] {
        assert!(recovery.dispatch(&host.app, kind).await.unwrap().is_none());
    }
}

/// Control's deletion, as a test double reports it.
#[derive(Debug, Default)]
struct Deletions {
    deleted: RefCell<BTreeSet<AppId>>,
    unavailable: RefCell<bool>,
}

impl AppLifecycle for Deletions {
    fn deleted<'a>(
        &'a self,
        apps: &'a [AppId],
    ) -> Pin<Box<dyn Future<Output = Result<BTreeSet<AppId>, Error>> + 'a>> {
        Box::pin(async move {
            if *self.unavailable.borrow() {
                return Err(Error::Unavailable);
            }
            Ok(apps
                .iter()
                .filter(|app| self.deleted.borrow().contains(*app))
                .cloned()
                .collect())
        })
    }
}

/// The lane closes an idle scope and abandons a deleted one in one pass, and
/// visits nothing when deletion state cannot be read.
async fn driver_lane(fixture: &Fixture) {
    let idle = host(fixture).await;
    let deleted = host(fixture).await;
    let unread = host(fixture).await;
    for host in [&idle, &deleted, &unread] {
        postpone_duties(fixture, &host.app).await;
    }
    compio::time::sleep(TICK * 5).await;
    let deletions = Rc::new(Deletions::default());
    deletions.deleted.borrow_mut().insert(deleted.app.clone());
    let driver_options = driver::Options {
        recovery: options(TICK, HOUR, HOUR, HOUR),
        ..driver::Options::default()
    };
    // A driver whose deletion source is down visits no closing candidate.
    *deletions.unavailable.borrow_mut() = true;
    let mut blind = support::driver_for(&unread.queue, driver_options, deletions.clone());
    let report = blind.tick().await.closing;
    assert_eq!(report.scan_error, Some(Error::Unavailable));
    assert_eq!(report.visited, 0);
    for host in [&idle, &deleted, &unread] {
        assert_eq!(host.state().await.state, ScopeState::Open);
    }
    *deletions.unavailable.borrow_mut() = false;
    // Under a longer idle window no scope is a candidate, deleted or not.
    let mut patient = support::driver_for(
        &idle.queue,
        driver::Options {
            recovery: options(HOUR, HOUR, HOUR, HOUR),
            ..driver::Options::default()
        },
        deletions.clone(),
    );
    let report = patient.tick().await.closing;
    assert_eq!((report.visited, report.scan_error), (0, None), "{report:?}");
    assert_eq!(deleted.state().await.state, ScopeState::Open);

    let mut driver = support::driver_for(&idle.queue, driver_options, deletions);
    let report = driver.tick().await.closing;
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(report.visited, report.completed);
    assert!(report.visited >= 3);
    assert_eq!(idle.state().await.state, ScopeState::Closing);
    assert_eq!(deleted.state().await.state, ScopeState::Abandoned);
    assert_eq!(duties(fixture, &deleted.app).await, 0);
    assert!(
        rows(
            fixture,
            "jobs",
            value!({"app_id":deleted.app.as_str(),"operation_kind":"close"})
        )
        .await
        .is_empty(),
        "a deleted app is abandoned, not closed"
    );
    let attempts = rows(
        fixture,
        "jobs",
        value!({"app_id":idle.app.as_str(),"operation_kind":"close"}),
    )
    .await;
    assert_eq!(attempts.len(), 1);
    // A later pass leaves the attempt pending and the abandoned scope alone.
    driver.tick().await;
    assert_eq!(
        rows(
            fixture,
            "jobs",
            value!({"app_id":idle.app.as_str(),"operation_kind":"close"})
        )
        .await
        .len(),
        1
    );
    assert_eq!(deleted.state().await.state, ScopeState::Abandoned);
    assert_eq!(unread.state().await.state, ScopeState::Closing);
}
