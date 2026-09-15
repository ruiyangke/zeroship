//! Scope retirement: the ingress epoch, the closing watermark and re-arm hooks.
#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native retirement fixtures stay on their compio runtime"
)]

#[allow(
    dead_code,
    reason = "shared queue fixtures also expose backend administration"
)]
mod support;

use futures::future::ready;
use std::{
    cell::{Cell, RefCell},
    future::Future,
    num::NonZeroU32,
    pin::Pin,
    time::{Duration, Instant},
};
use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, RegisterWorker, Revision, WorkerId, WorkerState,
    },
    workflow_jobs::{
        BroadcastId, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, Settlement,
        SettlementReceipt, SubmitJob,
    },
    workflow_policy::{AppPolicy, EstablishIngress, PolicyLeaseRequest},
};
use zeroship_data_orm::{
    orm::{Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    coordinator::{self, Coordinator, Placed},
    policy::{PolicyObservation, PolicySource},
    recovery::{self, DutyKind, Recovery, Responsibility, ScopeState},
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
    sqlite_establishment_advances_the_epoch_exactly_once,
    postgres_establishment_advances_the_epoch_exactly_once,
    establishment
);
case!(
    sqlite_archived_policy_refuses_establishment_but_not_closure,
    postgres_archived_policy_refuses_establishment_but_not_closure,
    archived
);
case!(
    sqlite_intent_producing_claims_reopen_a_retired_scope_once,
    postgres_intent_producing_claims_reopen_a_retired_scope_once,
    claim_rearm
);
case!(
    sqlite_worker_publication_reopens_a_retired_scope_once,
    postgres_worker_publication_reopens_a_retired_scope_once,
    publication_rearm
);
case!(
    sqlite_settlement_retires_only_drained_evidence,
    postgres_settlement_retires_only_drained_evidence,
    settlement
);
case!(
    sqlite_work_above_the_watermark_cancels_retirement,
    postgres_work_above_the_watermark_cancels_retirement,
    watermark
);
case!(
    sqlite_publication_above_the_watermark_cancels_retirement,
    postgres_publication_above_the_watermark_cancels_retirement,
    published_watermark
);
case!(
    sqlite_closing_waits_for_leases_and_maintenance,
    postgres_closing_waits_for_leases_and_maintenance,
    preconditions
);
case!(
    sqlite_lost_replies_and_redelivery_converge_on_one_retirement,
    postgres_lost_replies_and_redelivery_converge_on_one_retirement,
    lost_replies
);
case!(
    sqlite_closing_suspends_duties_until_its_timeout,
    postgres_closing_suspends_duties_until_its_timeout,
    no_starvation
);
case!(
    sqlite_activation_reopens_a_retired_scope_at_the_next_epoch,
    postgres_activation_reopens_a_retired_scope_at_the_next_epoch,
    activation
);
case!(
    sqlite_workers_cannot_publish_closure_or_maintenance,
    postgres_workers_cannot_publish_closure_or_maintenance,
    worker_denial
);

const KEY: &str = "verified-enrolled-key";
const LONG: Duration = Duration::from_secs(600);

fn revision(value: i64) -> Revision {
    value.try_into().unwrap()
}

/// A finite policy authority whose values a test can replace, as archive does.
#[derive(Debug)]
struct Source {
    observation: RefCell<PolicyObservation>,
}

impl Source {
    fn new(app: &AppId, policy: AppPolicy) -> Self {
        Self {
            observation: RefCell::new(
                PolicyObservation::new(app.clone(), revision(1), policy, Instant::now() + LONG)
                    .unwrap(),
            ),
        }
    }

    fn replace(&self, policy: AppPolicy) {
        let previous = self.observation.borrow().clone();
        *self.observation.borrow_mut() = PolicyObservation::new(
            previous.app_id().clone(),
            revision(previous.revision().get() + 1),
            policy,
            Instant::now() + LONG,
        )
        .unwrap();
    }
}

impl PolicySource for Source {
    fn observe<'a>(
        &'a self,
        _: &'a AppId,
    ) -> Pin<Box<dyn Future<Output = Result<PolicyObservation, Error>> + 'a>> {
        Box::pin(async move { Ok(self.observation.borrow().clone()) })
    }

    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, Error> {
        if self.observation.borrow().same_observation(observation) {
            Ok(observation.expires_at())
        } else {
            Err(Error::Unavailable)
        }
    }
}

/// The archive mask: admission, dispatch and ingress are all disabled.
fn archived_policy() -> AppPolicy {
    AppPolicy {
        admission: false,
        dispatch: false,
        ingress: false,
        ..AppPolicy::default()
    }
}

struct Host {
    queue: Queue,
    recovery: Recovery,
    coordinator: Coordinator,
    app: AppId,
    worker: WorkerId,
    scope: AssignedScope,
}

async fn host(fixture: &Fixture) -> Host {
    host_with(fixture, AppId::mint()).await
}

/// An activated scope at ingress epoch one, placed on one ready worker.
async fn host_with(fixture: &Fixture, app: AppId) -> Host {
    let queue = Queue::connect(
        fixture.binding(),
        fixture.url(),
        zeroship_workflow_manager::Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    let recovery = Recovery::new(
        queue.clone(),
        recovery::Options {
            interval: Duration::from_secs(3600),
            page_size: 16,
            closing_timeout: Duration::from_secs(3600),
            ..recovery::Options::default()
        },
    )
    .unwrap();
    let coordinator = support::coordinator(&queue, coordinator::Options::default());
    let (worker, scope) = place(&coordinator, &app).await;
    recovery
        .ensure(&app, &DeploymentId::mint(), revision(1))
        .await
        .unwrap();
    Host {
        queue,
        recovery,
        coordinator,
        app,
        worker,
        scope,
    }
}

/// Register an instance and let the manager place the app; the placement
/// names the instance selection chose, which need not be the new one.
async fn place(coordinator: &Coordinator, app: &AppId) -> (WorkerId, AssignedScope) {
    coordinator
        .register(
            &WorkerId::mint(),
            &RegisterWorker {
                capacity: NonZeroU32::new(4).unwrap(),
                state: WorkerState::Ready,
            },
        )
        .await
        .unwrap();
    let Placed::Assigned(assignment) = coordinator.place(app).await.unwrap() else {
        panic!("the app has an eligible worker");
    };
    (
        assignment.worker_id,
        AssignedScope {
            app_id: assignment.app_id,
            assignment_revision: assignment.revision,
        },
    )
}

impl Host {
    async fn state(&self) -> Responsibility {
        self.recovery
            .responsibility(&self.app)
            .await
            .unwrap()
            .expect("activated scope")
    }

    async fn lease(
        &self,
        source: &Source,
        establish_after: Option<i64>,
        ingress_used: bool,
    ) -> Result<Option<Revision>, Error> {
        let request = PolicyLeaseRequest {
            scope: self.scope.clone(),
            establish: establish_after.map(|after| EstablishIngress {
                after: (after > 0).then(|| revision(after)),
            }),
            ingress_used,
        };
        let grant = self
            .coordinator
            .policy_lease(&self.worker, KEY, &request, source, || {
                ready(Ok(self.worker.clone()))
            })
            .await?;
        let lease = grant.lease()?;
        assert_eq!(lease.ingress_epoch, grant.ingress_epoch());
        Ok(grant.ingress_epoch())
    }

    async fn claim_as(&self, worker: &WorkerId, scope: &AssignedScope) -> Option<DeliveryGrant> {
        self.coordinator
            .claim_job(worker, scope, || ready(Ok(worker.clone())))
            .await
            .unwrap()
    }

    async fn claim(&self) -> Option<DeliveryGrant> {
        self.claim_as(&self.worker, &self.scope).await
    }

    async fn settle_as(
        &self,
        worker: &WorkerId,
        grant: &DeliveryGrant,
        outcome: JobOutcome,
    ) -> Result<SettlementReceipt, Error> {
        self.coordinator
            .settle_job(
                worker,
                &Settlement {
                    delivery: grant.delivery().clone(),
                    outcome,
                    successors: Vec::new(),
                },
                || ready(Ok(worker.clone())),
            )
            .await
    }

    async fn settle(
        &self,
        grant: &DeliveryGrant,
        outcome: JobOutcome,
    ) -> Result<SettlementReceipt, Error> {
        self.settle_as(&self.worker, grant, outcome).await
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

    /// Close the open scope with drained evidence and no competing work.
    async fn retire(&self) -> Revision {
        let close = self.recovery.begin_close(&self.app).await.unwrap().unwrap();
        let grant = self.claim().await.expect("closure is delivered");
        assert_eq!(grant.delivery().job, close);
        self.settle(&grant, JobOutcome::Closed { drained: true })
            .await
            .unwrap();
        let retired = self.state().await;
        assert_eq!(retired.state, ScopeState::Retired);
        assert!(retired.close_job.is_none() && retired.closing_watermark.is_none());
        retired.ingress_epoch
    }
}

/// An intent-producing job with no executable prerequisite.
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

async fn duties(fixture: &Fixture, app: &AppId) -> Vec<Value> {
    rows(fixture, "recovery_duties", value!({"app_id":app.as_str()})).await
}

async fn patch(fixture: &Fixture, collection: &str, filter: Value, patch: Value) {
    let updated = fixture
        .database()
        .await
        .collection(collection)
        .unwrap()
        .execute(Operation::Update {
            filter,
            patch,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(updated, Output::Count(1)), "{updated:?}");
}

/// A due time shortly after the scope can retire, like a sleeping run's timer.
/// Job specifications are immutable, so availability comes from time alone.
fn soon() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    i64::try_from(now).unwrap() + 3_000
}

/// Wait on the clock until a timer published with [`soon`] has fallen due.
async fn fall_due(available_at: i64) {
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let remaining = available_at + 50 - i64::try_from(now).unwrap();
        if remaining <= 0 {
            return;
        }
        compio::time::sleep(Duration::from_millis(remaining.unsigned_abs())).await;
    }
}

/// Establishment reopens or advances only when asked, commits before the lease
/// and returns the same epoch to a retried request.
async fn establishment(fixture: &Fixture) {
    let host = host(fixture).await;
    let source = Source::new(&host.app, AppPolicy::default());
    let opened = host.state().await.active_at;
    compio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(host.lease(&source, None, false).await, Ok(Some(revision(1))));
    assert_eq!(host.state().await.active_at, opened, "a plain refresh is no activity");
    // Startup names no refused epoch and keeps the open one.
    assert_eq!(host.lease(&source, Some(0), false).await, Ok(Some(revision(1))));
    assert_eq!(host.state().await.active_at, opened);
    assert_eq!(host.lease(&source, None, true).await, Ok(Some(revision(1))));
    assert!(host.state().await.active_at > opened, "reported ingress is activity");
    let original_duties = duties(fixture, &host.app).await;
    assert_eq!(original_duties.len(), 2);

    // The creator refused epoch one while the manager still holds it open.
    assert_eq!(host.lease(&source, Some(1), false).await, Ok(Some(revision(2))));
    assert_eq!(host.lease(&source, Some(1), false).await, Ok(Some(revision(2))));
    assert_eq!(host.lease(&source, None, false).await, Ok(Some(revision(2))));
    assert_eq!(duties(fixture, &host.app).await, original_duties);
    assert_eq!(host.lease(&source, Some(3), false).await, Err(Error::Conflict));
    assert_eq!(host.state().await.ingress_epoch, revision(2));

    // Establishment cancels a closing attempt; its Close no longer matches.
    let stale = host.recovery.begin_close(&host.app).await.unwrap().unwrap();
    assert_eq!(
        stale.operation,
        JobOperation::Close {
            epoch: revision(2)
        }
    );
    assert_eq!(host.state().await.state, ScopeState::Closing);
    assert_eq!(host.lease(&source, None, false).await, Ok(Some(revision(2))));
    assert_eq!(host.lease(&source, Some(2), false).await, Ok(Some(revision(3))));
    let cancelled = host.state().await;
    assert_eq!(cancelled.state, ScopeState::Open);
    assert_eq!(cancelled.ingress_epoch, revision(3));
    assert!(cancelled.close_job.is_none() && cancelled.closing_watermark.is_none());
    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, stale);
    host.settle(&grant, JobOutcome::Closed { drained: true })
        .await
        .unwrap();
    assert_eq!(host.state().await, cancelled);
    assert_eq!(duties(fixture, &host.app).await, original_duties);

    // A plain refresh reports a retired scope without reopening it.
    assert_eq!(host.retire().await, revision(3));
    assert!(duties(fixture, &host.app).await.is_empty());
    assert_eq!(host.lease(&source, None, true).await, Ok(None));
    assert_eq!(host.state().await.state, ScopeState::Retired);
    assert!(duties(fixture, &host.app).await.is_empty());

    // Establishing after the retired epoch reopens once and recreates duties.
    assert_eq!(host.lease(&source, Some(3), false).await, Ok(Some(revision(4))));
    assert_eq!(host.lease(&source, Some(3), false).await, Ok(Some(revision(4))));
    let reopened = host.state().await;
    assert_eq!(reopened.state, ScopeState::Open);
    assert_eq!(reopened.ingress_epoch, revision(4));
    let recreated = duties(fixture, &host.app).await;
    assert_eq!(recreated.len(), 2);
    assert!(recreated
        .iter()
        .all(|duty| duty["pending_job_id"].is_null()));
    assert!(host
        .recovery
        .dispatch(&host.app, DutyKind::Reconcile)
        .await
        .unwrap()
        .is_some());
    assert_eq!(host.lease(&source, Some(4), false).await, Ok(Some(revision(5))));
    assert_eq!(duties(fixture, &host.app).await.len(), 2);
}

/// Archive masks admission, so ingress cannot reopen responsibility. Closure is
/// manager-origin and still delivered, drained and retired under that policy.
async fn archived(fixture: &Fixture) {
    let host = host(fixture).await;
    let source = Source::new(&host.app, archived_policy());
    assert_eq!(host.lease(&source, None, false).await, Ok(Some(revision(1))));
    let close = host.recovery.begin_close(&host.app).await.unwrap().unwrap();
    assert_eq!(
        close.operation,
        JobOperation::Close {
            epoch: revision(1)
        }
    );
    // Establishment is refused while closing; the attempt continues.
    assert_eq!(host.lease(&source, Some(1), false).await, Err(Error::Denied));
    let closing = host.state().await;
    assert_eq!(closing.state, ScopeState::Closing);
    let grant = host.claim().await.expect("closure is delivered under archive");
    assert_eq!(grant.delivery().job, close);
    host.settle(&grant, JobOutcome::Closed { drained: true })
        .await
        .unwrap();
    assert_eq!(host.state().await.state, ScopeState::Retired);
    assert_eq!(host.lease(&source, Some(1), false).await, Err(Error::Denied));
    assert_eq!(host.lease(&source, None, false).await, Ok(None));
    let retired = host.state().await;
    assert_eq!(retired.state, ScopeState::Retired);
    assert_eq!(retired.ingress_epoch, revision(1));
    assert!(duties(fixture, &host.app).await.is_empty());

    // Control: unarchive restores admission and the next establishment reopens.
    source.replace(AppPolicy::default());
    assert_eq!(host.lease(&source, Some(1), false).await, Ok(Some(revision(2))));
    assert_eq!(duties(fixture, &host.app).await.len(), 2);
}

/// A claimed intent-producing job restores responsibility before it executes;
/// maintenance and closure claims do not, and a rolled-back claim leaves none.
/// The jobs were published before retirement and fall due after it, like a
/// sleeping run's timer, so unsettled ready jobs do not block retirement.
async fn claim_rearm(fixture: &Fixture) {
    let host = host(fixture).await;
    let due = soon();
    let maintenance = JobSpec {
        id: JobId::mint(),
        app_id: host.app.clone(),
        operation: JobOperation::Reconcile {},
        available_at: due.try_into().unwrap(),
    };
    let first = fanout(&host.app, due);
    let second = fanout(&host.app, due);
    host.queue.submit(&maintenance).await.unwrap();
    host.publish(&first).await.unwrap();
    host.publish(&second).await.unwrap();
    assert_eq!(host.retire().await, revision(1));
    fall_due(due).await;

    // Control: a maintenance claim cannot produce intents and does not reopen.
    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, maintenance);
    assert_eq!(host.state().await.state, ScopeState::Retired);
    host.settle(&grant, JobOutcome::Completed {}).await.unwrap();
    assert!(duties(fixture, &host.app).await.is_empty());

    // A claim that fails its final authorization rolls its reopen back.
    let checks = Cell::new(0);
    let refused = host
        .coordinator
        .claim_job(&host.worker, &host.scope, || {
            checks.set(checks.get() + 1);
            ready(if checks.get() >= 2 {
                Err(Error::Denied)
            } else {
                Ok(host.worker.clone())
            })
        })
        .await;
    assert_eq!(refused.unwrap_err(), Error::Denied);
    assert_eq!(checks.get(), 2, "the refusal came after the reopen");
    assert_eq!(host.state().await.state, ScopeState::Retired);
    assert!(duties(fixture, &host.app).await.is_empty());

    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, first);
    let reopened = host.state().await;
    assert_eq!(reopened.state, ScopeState::Open);
    assert_eq!(reopened.ingress_epoch, revision(2));
    assert_eq!(duties(fixture, &host.app).await.len(), 2);
    host.settle(&grant, JobOutcome::Completed {}).await.unwrap();

    // Later intent-producing claims find the scope open and only record activity.
    compio::time::sleep(Duration::from_millis(5)).await;
    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, second);
    let later = host.state().await;
    assert!(later.active_at > reopened.active_at, "a claim is activity");
    assert_eq!(Responsibility { active_at: reopened.active_at, ..later }, reopened);
    assert_eq!(duties(fixture, &host.app).await.len(), 2);
}

/// Worker publication to a retired scope reopens it once; the exact retry and
/// later publications find it open.
async fn publication_rearm(fixture: &Fixture) {
    let host = host(fixture).await;
    assert_eq!(host.retire().await, revision(1));
    let job = fanout(&host.app, FUTURE);
    let checks = Cell::new(0);
    let refused = host
        .coordinator
        .submit_job(
            &host.worker,
            &SubmitJob {
                scope: host.scope.clone(),
                job: job.clone(),
            },
            || {
                checks.set(checks.get() + 1);
                ready(if checks.get() >= 2 {
                    Err(Error::Denied)
                } else {
                    Ok(host.worker.clone())
                })
            },
        )
        .await;
    assert_eq!(refused.unwrap_err(), Error::Denied);
    assert_eq!(host.state().await.state, ScopeState::Retired);
    assert!(duties(fixture, &host.app).await.is_empty());

    assert_eq!(host.publish(&job).await.unwrap(), job);
    let reopened = host.state().await;
    assert_eq!(reopened.state, ScopeState::Open);
    assert_eq!(reopened.ingress_epoch, revision(2));
    assert_eq!(duties(fixture, &host.app).await.len(), 2);
    assert_eq!(host.publish(&job).await.unwrap(), job);
    compio::time::sleep(Duration::from_millis(5)).await;
    host.publish(&fanout(&host.app, FUTURE)).await.unwrap();
    let later = host.state().await;
    assert!(later.active_at > reopened.active_at, "publication is activity");
    assert_eq!(Responsibility { active_at: reopened.active_at, ..later }, reopened);
    assert_eq!(duties(fixture, &host.app).await.len(), 2);
}

/// Undrained evidence returns the attempt to open; drained evidence retires
/// responsibility and keeps the scope row as the epoch's tombstone.
async fn settlement(fixture: &Fixture) {
    let host = host(fixture).await;
    let original_duties = duties(fixture, &host.app).await;
    let close = host.recovery.begin_close(&host.app).await.unwrap().unwrap();
    assert_eq!(
        host.recovery.begin_close(&host.app).await.unwrap(),
        Some(close.clone()),
        "replicas converge on one attempt"
    );
    let closing = host.state().await;
    assert_eq!(closing.state, ScopeState::Closing);
    assert_eq!(closing.close_job, Some(close.id.clone()));
    let grant = host.claim().await.unwrap();
    assert_eq!(
        host.settle(&grant, JobOutcome::Completed {}).await,
        Err(Error::Invalid),
        "closure settles only with closed evidence"
    );
    host.settle(&grant, JobOutcome::Closed { drained: false })
        .await
        .unwrap();
    let reopened = host.state().await;
    assert_eq!(reopened.state, ScopeState::Open);
    assert_eq!(reopened.ingress_epoch, revision(1));
    assert!(reopened.close_job.is_none() && reopened.closing_watermark.is_none());
    assert_eq!(duties(fixture, &host.app).await, original_duties);

    assert_eq!(host.retire().await, revision(1));
    assert!(duties(fixture, &host.app).await.is_empty());
    assert!(host
        .recovery
        .dispatch(&host.app, DutyKind::Collect)
        .await
        .unwrap()
        .is_none());
    assert!(host.recovery.begin_close(&host.app).await.unwrap().is_none());
    let tombstone = rows(fixture, "recovery_scopes", value!({"id":host.app.as_str()})).await;
    assert_eq!(tombstone.len(), 1);
    assert_eq!(tombstone[0]["state"], value!("retired"));
}

/// A job claimed after closing began can commit creator intents after the Close
/// fence, because delivered work is not fenced by the ingress epoch. Its
/// dispatch ticket above the watermark must cancel the retirement.
async fn watermark(fixture: &Fixture) {
    // Claimed after closing began, settled before the Close settles.
    let host = host(fixture).await;
    let delivered = fanout(&host.app, 0);
    host.publish(&delivered).await.unwrap();
    let close = host.recovery.begin_close(&host.app).await.unwrap().unwrap();
    let job = host.claim().await.unwrap();
    assert_eq!(job.delivery().job, delivered);
    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, close);
    assert_eq!(
        host.state().await.state,
        ScopeState::Closing,
        "a claim during closing leaves the attempt to its watermark"
    );
    host.settle(&job, JobOutcome::Completed {}).await.unwrap();
    host.settle(&grant, JobOutcome::Closed { drained: true })
        .await
        .unwrap();
    let kept = host.state().await;
    assert_eq!(kept.state, ScopeState::Open, "retired above the watermark");
    assert_eq!(kept.ingress_epoch, revision(1));
    assert_eq!(duties(fixture, &host.app).await.len(), 2);
    retire_without_later_work(fixture).await;
}

/// A worker publication during closing, such as a delivered job's outbox
/// retry, also holds a ticket above the watermark and cancels the retirement.
async fn published_watermark(fixture: &Fixture) {
    let host = host(fixture).await;
    let close = host.recovery.begin_close(&host.app).await.unwrap().unwrap();
    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, close);
    host.publish(&fanout(&host.app, FUTURE)).await.unwrap();
    assert_eq!(host.state().await.state, ScopeState::Closing);
    host.settle(&grant, JobOutcome::Closed { drained: true })
        .await
        .unwrap();
    assert_eq!(
        host.state().await.state,
        ScopeState::Open,
        "retired above the watermark"
    );
    retire_without_later_work(fixture).await;
}

/// Control: without work after closing began, the same evidence retires.
async fn retire_without_later_work(fixture: &Fixture) {
    let host = host(fixture).await;
    let earlier = fanout(&host.app, FUTURE);
    host.publish(&earlier).await.unwrap();
    assert_eq!(host.retire().await, revision(1));
}

/// Closing needs no live lease and no pending maintenance job, including an
/// earlier attempt's unsettled Close.
async fn preconditions(fixture: &Fixture) {
    let host = host(fixture).await;
    host.publish(&fanout(&host.app, 0)).await.unwrap();
    let leased = host.claim().await.unwrap();
    assert!(host.recovery.begin_close(&host.app).await.unwrap().is_none());
    assert_eq!(host.state().await.state, ScopeState::Open);
    host.settle(&leased, JobOutcome::Completed {}).await.unwrap();

    let pending = host
        .recovery
        .dispatch(&host.app, DutyKind::Collect)
        .await
        .unwrap()
        .unwrap();
    assert!(host.recovery.begin_close(&host.app).await.unwrap().is_none());
    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, pending);
    host.settle(&grant, JobOutcome::Completed {}).await.unwrap();

    let stale = host.recovery.begin_close(&host.app).await.unwrap().unwrap();
    patch(
        fixture,
        "jobs",
        value!({"id":stale.id.as_str()}),
        value!({"created_at":0}),
    )
    .await;
    assert!(host.recovery.expire_close(&host.app).await.unwrap());
    assert!(
        host.recovery.begin_close(&host.app).await.unwrap().is_none(),
        "an unsettled earlier Close blocks another attempt"
    );
    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, stale);
    host.settle(&grant, JobOutcome::Closed { drained: true })
        .await
        .unwrap();
    assert_eq!(host.state().await.state, ScopeState::Open);
    assert_eq!(host.retire().await, revision(1));
}

/// A lost Close acknowledgement, a crash with redelivery to another worker and
/// a lost settlement reply all converge on one retirement.
async fn lost_replies(fixture: &Fixture) {
    let host = host(fixture).await;
    let close = host.recovery.begin_close(&host.app).await.unwrap().unwrap();
    let crashed = host.claim().await.unwrap();
    assert_eq!(crashed.delivery().job, close);
    // The first worker committed its creator receipt, then crashed before
    // settling. Its manager lease lapses and the job is redelivered.
    patch(
        fixture,
        "jobs",
        value!({"id":close.id.as_str()}),
        value!({"lease_deadline":0}),
    )
    .await;
    // The crashed instance stops taking placements, so the manager gives the
    // app an owner that can take the redelivery over.
    host.coordinator
        .register(
            &host.worker,
            &RegisterWorker {
                capacity: NonZeroU32::new(4).unwrap(),
                state: WorkerState::Draining,
            },
        )
        .await
        .unwrap();
    let (other, other_scope) = place(&host.coordinator, &host.app).await;
    assert_ne!(other, host.worker);
    let redelivered = host.claim_as(&other, &other_scope).await.unwrap();
    assert_eq!(redelivered.delivery().job, close);
    assert_eq!(redelivered.delivery().attempt.get(), 2);
    let receipt = host
        .settle_as(&other, &redelivered, JobOutcome::Closed { drained: true })
        .await
        .unwrap();
    let retired = host.state().await;
    assert_eq!(retired.state, ScopeState::Retired);
    // The lost settlement reply replays the receipt without a second effect.
    assert_eq!(
        host.settle_as(&other, &redelivered, JobOutcome::Closed { drained: true })
            .await,
        Ok(receipt)
    );
    assert_eq!(host.state().await, retired);
    // The crashed attempt cannot settle a second time.
    assert_eq!(
        host.settle(&crashed, JobOutcome::Closed { drained: true })
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(host.state().await, retired);
    assert!(duties(fixture, &host.app).await.is_empty());
}

/// A duty that falls due during closing is not dispatched, so it cannot cancel
/// the attempt. After the closing timeout returns the scope to open, it is.
async fn no_starvation(fixture: &Fixture) {
    let host = host(fixture).await;
    let close = host.recovery.begin_close(&host.app).await.unwrap().unwrap();
    for kind in [DutyKind::Reconcile, DutyKind::Collect] {
        assert!(host.recovery.due(kind, None).await.unwrap().is_empty());
        assert!(host.recovery.dispatch(&host.app, kind).await.unwrap().is_none());
    }
    let grant = host.claim().await.unwrap();
    assert_eq!(grant.delivery().job, close);
    host.settle(&grant, JobOutcome::Closed { drained: true })
        .await
        .unwrap();
    assert_eq!(host.state().await.state, ScopeState::Retired);

    // Control: a closing attempt that outlives its timeout reopens the duties.
    let host = self::host(fixture).await;
    let close = host.recovery.begin_close(&host.app).await.unwrap().unwrap();
    assert!(!host.recovery.expire_close(&host.app).await.unwrap());
    assert!(host
        .recovery
        .dispatch(&host.app, DutyKind::Reconcile)
        .await
        .unwrap()
        .is_none());
    patch(
        fixture,
        "jobs",
        value!({"id":close.id.as_str()}),
        value!({"created_at":0}),
    )
    .await;
    assert!(host.recovery.expire_close(&host.app).await.unwrap());
    let reopened = host.state().await;
    assert_eq!(reopened.state, ScopeState::Open);
    assert_eq!(reopened.ingress_epoch, revision(1));
    assert_eq!(
        host.recovery.due(DutyKind::Reconcile, None).await.unwrap(),
        std::slice::from_ref(&host.app)
    );
    let duty = host
        .recovery
        .dispatch(&host.app, DutyKind::Reconcile)
        .await
        .unwrap()
        .expect("the reopened duty dispatches");
    assert_eq!(duty.operation, JobOperation::Reconcile {});
    // The timed-out Close settles without touching the reopened scope.
    let first = host.claim().await.unwrap();
    assert_eq!(first.delivery().job, close);
    host.settle(&first, JobOutcome::Closed { drained: true })
        .await
        .unwrap();
    assert_eq!(host.state().await, reopened);
}

/// Workers cannot publish or name as a successor any closure or maintenance job.
async fn worker_denial(fixture: &Fixture) {
    let host = host(fixture).await;
    for operation in [
        JobOperation::Close {
            epoch: revision(1),
        },
        JobOperation::Reconcile {},
        JobOperation::Collect {},
    ] {
        let job = JobSpec {
            id: JobId::mint(),
            app_id: host.app.clone(),
            operation,
            available_at: 0.try_into().unwrap(),
        };
        assert_eq!(host.publish(&job).await, Err(Error::Denied));
        host.publish(&fanout(&host.app, FUTURE)).await.unwrap();
    }
    let close = JobSpec {
        id: JobId::mint(),
        app_id: host.app.clone(),
        operation: JobOperation::Close {
            epoch: revision(1),
        },
        available_at: 0.try_into().unwrap(),
    };
    assert_eq!(host.queue.submit(&close).await, Err(Error::Invalid));
    let job = fanout(&host.app, 0);
    host.publish(&job).await.unwrap();
    let grant = host.claim().await.unwrap();
    let settlement = Settlement {
        delivery: grant.delivery().clone(),
        outcome: JobOutcome::Completed {},
        successors: vec![close],
    };
    assert_eq!(
        host.coordinator
            .settle_job(&host.worker, &settlement, || ready(Ok(host.worker.clone())))
            .await,
        Err(Error::Denied)
    );
    assert_eq!(host.state().await.state, ScopeState::Open);
}

/// Wait until `waiters` manager sessions are queued behind a lock. The admin
/// polls from inside its own open transaction, where the server would otherwise
/// keep serving the activity statistics it cached at first access.
async fn blocked_manager(admin: &compio_postgres::Client, predicate: &str, waiters: i64) {
    let sql = format!(
        "SELECT count(DISTINCT a.pid) FROM pg_locks l \
         JOIN pg_stat_activity a ON a.pid=l.pid \
         WHERE a.usename='workflow_manager_test' AND NOT l.granted AND ({predicate})"
    );
    compio::time::timeout(Duration::from_secs(4), async {
        loop {
            admin
                .batch_execute("SELECT pg_stat_clear_snapshot()")
                .await
                .unwrap();
            if admin.query(&sql, &[]).await.unwrap()[0].get::<_, i64>(0) >= waiters {
                return;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("manager operations must reach the app lock");
}

/// Read a scope's state through the administrator, whose transaction does not
/// wait on the manager thread's open transaction.
async fn admin_state(admin: &compio_postgres::Client, app: &AppId) -> String {
    admin
        .query(
            "SELECT state FROM workflow_manager.recovery_scopes WHERE id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap()[0]
        .get::<_, String>(0)
}

/// Claim-time reopen and establishment both serialize on the queue app lock the
/// claim transaction already holds, so the reopen needs no lock outside the
/// app-lock order. The establishment runs on a second manager replica with its
/// own runtime and connections, so both transactions really wait at the
/// database. Released together, they reopen exactly once.
#[compio::test]
async fn postgres_claim_and_establishment_share_the_app_lock_and_reopen_once() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let host = host(&fixture).await;
    let due = soon();
    let job = fanout(&host.app, due);
    host.publish(&job).await.unwrap();
    assert_eq!(host.retire().await, revision(1));
    fall_due(due).await;
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    admin.batch_execute("BEGIN").await.unwrap();
    let locked = admin
        .query(
            "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
            &[&host.app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(locked.len(), 1);
    let replica = replica_establishment(&fixture, &host, 1);
    let release = async {
        blocked_manager(admin, BLOCKED_BY_ADMIN, 2).await;
        assert_eq!(admin_state(admin, &host.app).await, "retired");
        admin.batch_execute("ROLLBACK").await.unwrap();
    };
    let (claimed, ()) = futures::join!(host.claim(), release);
    assert_eq!(claimed.unwrap().delivery().job, job);
    assert_eq!(replica.join().unwrap(), Ok(Some(revision(2))));
    let reopened = host.state().await;
    assert_eq!(reopened.state, ScopeState::Open);
    assert_eq!(reopened.ingress_epoch, revision(2));
    assert_eq!(duties(&fixture, &host.app).await.len(), 2);
}

/// A manager session queued behind the administrator's row lock on the app's
/// queue scope. The first waiter waits on that transaction; later waiters queue
/// on the first waiter's tuple lock.
const BLOCKED_BY_ADMIN: &str = "cardinality(pg_blocking_pids(a.pid)) > 0";

/// Establish after `after` from a separate manager replica on its own thread.
fn replica_establishment(
    fixture: &Fixture,
    host: &Host,
    after: i64,
) -> std::thread::JoinHandle<Result<Option<Revision>, Error>> {
    let binding = fixture.binding();
    let url = fixture.url().to_owned();
    let app = host.app.clone();
    let worker = host.worker.clone();
    let scope = host.scope.clone();
    std::thread::spawn(move || {
        compio::runtime::Runtime::new().unwrap().block_on(async move {
            let queue = Queue::connect(
                binding,
                &url,
                zeroship_workflow_manager::Options::default(),
                support::synthetic_holds(),
            )
            .await
            .unwrap();
            let coordinator = support::coordinator(&queue, coordinator::Options::default());
            let source = Source::new(&app, AppPolicy::default());
            let request = PolicyLeaseRequest {
                scope,
                establish: Some(EstablishIngress {
                    after: Some(revision(after)),
                }),
                ingress_used: false,
            };
            coordinator
                .policy_lease(&worker, KEY, &request, &source, || {
                    ready(Ok(worker.clone()))
                })
                .await
                .map(|grant| grant.ingress_epoch())
        })
    })
}

/// Establishment racing a drained Close settlement: whichever takes the app lock
/// first, the scope ends open above the closed epoch with its duties.
#[compio::test]
async fn postgres_establishment_and_close_settlement_serialize_in_both_orders() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    for establish_first in [true, false] {
        let host = host(&fixture).await;
        let source = Source::new(&host.app, AppPolicy::default());
        host.recovery.begin_close(&host.app).await.unwrap().unwrap();
        let grant = host.claim().await.unwrap();
        admin.batch_execute("BEGIN").await.unwrap();
        admin
            .query(
                "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
                &[&host.app.as_str()],
            )
            .await
            .unwrap();
        let first = Cell::new(None);
        let release = async {
            blocked_manager(admin, BLOCKED_BY_ADMIN, 1).await;
            admin.batch_execute("ROLLBACK").await.unwrap();
        };
        if establish_first {
            let (established, ()) =
                futures::join!(host.lease(&source, Some(1), false), release);
            assert_eq!(established, Ok(Some(revision(2))));
            first.set(Some("establishment"));
            host.settle(&grant, JobOutcome::Closed { drained: true })
                .await
                .unwrap();
        } else {
            let (settled, ()) = futures::join!(
                host.settle(&grant, JobOutcome::Closed { drained: true }),
                release
            );
            settled.unwrap();
            first.set(Some("settlement"));
            assert_eq!(host.state().await.state, ScopeState::Retired);
            assert_eq!(host.lease(&source, Some(1), false).await, Ok(Some(revision(2))));
        }
        let converged = host.state().await;
        assert_eq!(converged.state, ScopeState::Open, "{:?}", first.get());
        assert_eq!(converged.ingress_epoch, revision(2));
        assert_eq!(duties(&fixture, &host.app).await.len(), 2);
    }
}

/// A newer activation of a retired scope reopens it at the next epoch with a
/// fresh duty pair; replaying that activation changes nothing further.
async fn activation(fixture: &Fixture) {
    let host = host(fixture).await;
    assert_eq!(host.retire().await, revision(1));
    let deployment = DeploymentId::mint();
    host.recovery
        .ensure(&host.app, &deployment, revision(2))
        .await
        .unwrap();
    let reopened = host.state().await;
    assert_eq!(reopened.state, ScopeState::Open);
    assert_eq!(reopened.ingress_epoch, revision(2));
    assert_eq!(duties(fixture, &host.app).await.len(), 2);
    host.recovery
        .ensure(&host.app, &deployment, revision(2))
        .await
        .unwrap();
    assert_eq!(host.state().await, reopened);
    assert_eq!(duties(fixture, &host.app).await.len(), 2);
    // Control: a stale activation still conflicts and cannot reopen anything.
    assert_eq!(host.retire().await, revision(2));
    assert_eq!(
        host.recovery
            .ensure(&host.app, &DeploymentId::mint(), revision(1))
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(host.state().await.state, ScopeState::Retired);
    assert!(duties(fixture, &host.app).await.is_empty());
}
