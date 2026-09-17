#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native fixtures stay on their compio runtime"
)]

#[allow(
    dead_code,
    reason = "source fixture also supports latest selection tests"
)]
#[path = "support/latest.rs"]
mod latest_support;
#[allow(
    dead_code,
    reason = "shared queue fixtures also expose backend administration"
)]
mod support;

use std::{cell::Cell, future::ready, num::NonZeroU32, time::Duration};
use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME},
    workflow_coordination::{
        AssignedScope, Assignment, ManageRun, ManagementOperation, ManagementOutcome,
        RegisterWorker, RegisteredWorker, ReleaseReason, ReleaseScope, RequestId, RunId,
        RunOperation, RunState, WorkerId, WorkerState,
    },
    workflow_jobs::{
        BroadcastId, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, PropagationId,
        Settlement, SubmitJob,
    },
    workflow_schedules::ScheduleId,
};
use zeroship_data_orm::{
    orm::{Database, Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    coordinator::{Coordinator, Options, Placed},
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

#[path = "coordinator/draining.rs"]
mod draining;

case!(
    sqlite_competing_app_assignments_respect_worker_capacity,
    postgres_competing_app_assignments_respect_worker_capacity,
    competing_assignments
);
case!(
    sqlite_concurrent_worker_registration_preserves_identity_and_assignments,
    postgres_concurrent_worker_registration_preserves_identity_and_assignments,
    concurrent_worker_registration
);
case!(
    sqlite_management_receipts_remain_scoped_across_native_hosts,
    postgres_management_receipts_remain_scoped_across_native_hosts,
    management_receipts
);
case!(
    sqlite_claim_uses_stored_assignment_authority,
    postgres_claim_uses_stored_assignment_authority,
    claim_authority
);

async fn host(fixture: &Fixture, options: Options) -> (Coordinator, Queue) {
    let queue = Queue::connect(
        fixture.binding(),
        fixture.url(),
        QueueOptions::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    let coordinator = support::coordinator(&queue, options);
    (coordinator, queue)
}

async fn register(coordinator: &Coordinator, worker: &WorkerId, capacity: u32) {
    let request = RegisterWorker {
        capacity: NonZeroU32::new(capacity).unwrap(),
        state: WorkerState::Ready,
    };
    let registered = coordinator.register(worker, &request).await.unwrap();
    assert_eq!(&registered.worker_id, worker);
    assert_eq!(registered.capacity, request.capacity);
    assert_eq!(registered.state, request.state);
    assert!(registered.expires_at.get() > 0);
}

/// The manager selects the zone's one eligible worker for the app.
async fn place(coordinator: &Coordinator, app: &AppId) -> Assignment {
    match coordinator.place(app).await.unwrap() {
        Placed::Assigned(assignment) => assignment,
        other => panic!("expected a placement: {other:?}"),
    }
}

/// A worker gives up its placement, so the next visit places the app again
/// under the next revision.
async fn relinquish(coordinator: &Coordinator, assignment: &Assignment) {
    coordinator
        .release(
            &assignment.worker_id,
            &ReleaseScope {
                request_id: RequestId::mint(),
                app_id: assignment.app_id.clone(),
                assignment_revision: assignment.revision,
                reason: ReleaseReason::Relinquished,
            },
        )
        .await
        .unwrap();
}

fn scope(assignment: &Assignment) -> AssignedScope {
    AssignedScope {
        app_id: assignment.app_id.clone(),
        assignment_revision: assignment.revision,
    }
}

fn command(app: &AppId) -> ManageRun {
    ManageRun {
        request_id: RequestId::mint(),
        app_id: app.clone(),
        run_id: RunId::mint(),
        command: ManagementOperation::Transition {
            operation: RunOperation::Pause,
        },
    }
}

async fn rows(database: &Database, table: &str, filter: Value) -> Vec<Value> {
    let Output::Rows { rows, .. } = database
        .collection(table)
        .unwrap()
        .find(filter, value!({"limit":16,"orderBy":{"id":1}}))
        .await
        .unwrap()
    else {
        panic!("metadata query returned a count");
    };
    rows
}

async fn row(database: &Database, table: &str, filter: Value) -> Value {
    let mut rows = rows(database, table, filter).await;
    assert_eq!(rows.len(), 1, "expected one scoped {table} row");
    rows.pop().unwrap()
}

async fn update(database: &Database, table: &str, filter: Value, patch: Value) {
    let output = database
        .collection(table)
        .unwrap()
        .execute(Operation::Update {
            filter,
            patch,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(output, Output::Count(1)), "{output:?}");
}

/// Two replicas place two apps on one single-slot worker at once. The worker
/// row serializes capacity, so exactly one placement is admitted and the other
/// app is left unplaced for the capacity lane.
async fn competing_assignments(fixture: &Fixture) {
    let (left, queue) = host(fixture, Options::default()).await;
    let (right, _) = host(fixture, Options::default()).await;
    let worker = WorkerId::mint();
    register(&left, &worker, 1).await;
    let (first, second) = (AppId::mint(), AppId::mint());
    queue.register_scope(&first).await.unwrap();
    queue.register_scope(&second).await.unwrap();
    let (first_result, second_result) = futures::join!(left.place(&first), right.place(&second));
    let (accepted, rejected, assignment) =
        match (first_result.unwrap(), second_result.unwrap()) {
            (Placed::Assigned(assignment), Placed::Unplaced(_)) => (&first, &second, assignment),
            (Placed::Unplaced(_), Placed::Assigned(assignment)) => (&second, &first, assignment),
            results => panic!("competing placements must obey capacity: {results:?}"),
        };
    assert_eq!(right.place(accepted).await.unwrap(), Placed::Owned);
    assert!(matches!(
        left.place(rejected).await.unwrap(),
        Placed::Unplaced(_)
    ));
    assert_eq!(
        left.assignments(&worker, None).await.unwrap(),
        vec![assignment.clone()]
    );
    let database = fixture.database().await;
    let stored = rows(
        &database,
        "assignments",
        value!({"worker_id":worker.as_str()}),
    )
    .await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0]["app_id"], value!(assignment.app_id.as_str()));
    assert_eq!(stored[0]["revision"], value!(assignment.revision.get()));
}

async fn concurrent_worker_registration(fixture: &Fixture) {
    let hosts = futures::future::join_all((0..4).map(|_| host(fixture, Options::default())))
        .await
        .into_iter()
        .map(|(coordinator, _)| coordinator)
        .collect::<Vec<_>>();
    let database = fixture.database().await;
    for round in 0..16 {
        let worker = WorkerId::mint();
        register_workers_together(fixture, &hosts, &worker, round).await;
        let worker_filter = value!({"id":worker.as_str()});
        let initial = row(&database, "workers", worker_filter.clone()).await;
        assert_eq!(initial["id"], value!(worker.as_str()));
        assert_eq!(initial["lock_version"], value!(0));
        let assignment = place(&hosts[0], &AppId::mint()).await;
        let assignment_filter = value!({"app_id":assignment.app_id.as_str()});
        let before = row(&database, "assignments", assignment_filter.clone()).await;
        update(
            &database,
            "workers",
            worker_filter.clone(),
            value!({"lock_version":7}),
        )
        .await;
        register_workers_together(fixture, &hosts, &worker, round).await;
        let registered = row(&database, "workers", worker_filter).await;
        assert_eq!(registered["id"], initial["id"]);
        assert_eq!(registered["lock_version"], value!(7));
        assert_eq!(registered["capacity"], value!(1));
        assert_eq!(registered["state"], value!("ready"));
        assert_eq!(
            row(&database, "assignments", assignment_filter).await,
            before
        );
        assert_eq!(
            hosts[1].assignments(&worker, None).await.unwrap(),
            vec![assignment]
        );
        assert!(matches!(
            hosts[2].place(&AppId::mint()).await.unwrap(),
            Placed::Unplaced(_)
        ));
    }
}

async fn register_workers_together(
    fixture: &Fixture,
    hosts: &[Coordinator],
    worker: &WorkerId,
    round: usize,
) {
    let request = RegisterWorker {
        capacity: NonZeroU32::new(1).unwrap(),
        state: WorkerState::Ready,
    };
    let requests = vec![request.clone(); hosts.len()];
    let results = registration_race(fixture, hosts, worker, &requests).await;
    for (host, result) in results.into_iter().enumerate() {
        let registered = result.unwrap_or_else(|error| {
            panic!("worker registration round {round}, host {host}: {error:?}")
        });
        assert_eq!(&registered.worker_id, worker);
        assert_eq!(registered.capacity, request.capacity);
        assert_eq!(registered.state, request.state);
        assert!(registered.expires_at.get() > 0);
    }
}

async fn registration_race(
    fixture: &Fixture,
    hosts: &[Coordinator],
    worker: &WorkerId,
    requests: &[RegisterWorker],
) -> Vec<Result<RegisteredWorker, Error>> {
    assert!(!hosts.is_empty());
    assert_eq!(hosts.len(), requests.len());
    let registrations = futures::future::join_all(
        hosts
            .iter()
            .zip(requests)
            .map(|(host, request)| host.register(worker, request)),
    );
    if let Admin::Postgres(admin) = &fixture.admin {
        admin
            .batch_execute("BEGIN; LOCK TABLE workflow_manager.workers IN SHARE MODE")
            .await
            .unwrap();
        let release = async {
            compio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let blocked: i64 = admin.query(
                        "SELECT count(DISTINCT l.pid) FROM pg_locks l WHERE NOT l.granted \
                         AND l.mode='RowExclusiveLock' AND l.relation='workflow_manager.workers'::regclass",
                        &[],
                    ).await.unwrap()[0].get(0);
                    if usize::try_from(blocked).unwrap() == hosts.len() {
                        break;
                    }
                    compio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("independent worker registrations must reach the shared table barrier");
            admin.batch_execute("COMMIT").await.unwrap();
        };
        let (results, ()) = futures::join!(registrations, release);
        results
    } else {
        registrations.await
    }
}

async fn management_receipts(fixture: &Fixture) {
    let (coordinator, _) = host(
        fixture,
        Options {
            max_pending_management: 1,
            ..Options::default()
        },
    )
    .await;
    let source = latest_support::Source::new(fixture).await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 2).await;
    let app = AppId::mint();
    let foreign = AppId::mint();
    let assignment = place(&coordinator, &app).await;
    let other = place(&coordinator, &foreign).await;
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let request = command(&app);
    accept_management_receipt(&coordinator, &source, &request).await;
    assert_eq!(
        coordinator
            .management_receipt(&foreign, &request.request_id)
            .await
            .unwrap(),
        None
    );
    assert!(coordinator
        .claim_job(&worker, &scope(&other), support::delivery_ceiling(), || ready(Ok(worker.clone())))
        .await
        .unwrap()
        .is_none());
    let delivery = coordinator
        .claim_job(&worker, &scope(&assignment), support::delivery_ceiling(), || ready(Ok(worker.clone())))
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    let database = fixture.database().await;
    let stored = row(
        &database,
        "management",
        value!({"request_id":request.request_id.as_str()}),
    )
    .await;
    assert_eq!(stored["id"], value!(delivery.job.id.as_str()));
    let settlement = Settlement {
        delivery,
        outcome: JobOutcome::Management {
            outcome: ManagementOutcome::Applied {
                state: RunState::Paused,
            },
        },
        successors: vec![],
    };
    replay_management_receipt(
        fixture,
        &coordinator,
        &source,
        &worker,
        &request,
        &settlement,
    )
    .await;
    assert!(coordinator
        .manage(&actor, &command(&app), &source.latest)
        .await
        .unwrap()
        .outcome
        .is_none());
}

async fn accept_management_receipt(
    coordinator: &Coordinator,
    source: &latest_support::Source,
    request: &ManageRun,
) {
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    assert_eq!(
        coordinator
            .manage(
                &service_issuer(WORKER_SERVICE_NAME).unwrap(),
                request,
                &source.latest
            )
            .await,
        Err(Error::Denied)
    );
    let pending = coordinator
        .manage(&actor, request, &source.latest)
        .await
        .unwrap();
    assert_eq!(pending.outcome, None);
    assert_eq!(
        coordinator
            .manage(&actor, request, &source.latest)
            .await
            .unwrap(),
        pending
    );
    assert_eq!(
        coordinator
            .manage(&actor, &command(&request.app_id), &source.latest)
            .await,
        Err(Error::Capacity)
    );
    let changed = ManageRun {
        run_id: RunId::mint(),
        ..request.clone()
    };
    assert_eq!(
        coordinator.manage(&actor, &changed, &source.latest).await,
        Err(Error::Conflict)
    );
}

async fn replay_management_receipt(
    fixture: &Fixture,
    coordinator: &Coordinator,
    source: &latest_support::Source,
    worker: &WorkerId,
    request: &ManageRun,
    settlement: &Settlement,
) {
    assert_eq!(
        coordinator
            .settle_job(&WorkerId::mint(), settlement, || ready(Ok(worker.clone())))
            .await,
        Err(Error::Denied)
    );
    let receipt = coordinator
        .settle_job(worker, settlement, || ready(Ok(worker.clone())))
        .await
        .unwrap();
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let app = &request.app_id;
    let (reopened, _) = host(fixture, Options::default()).await;
    assert_eq!(
        reopened
            .settle_job(worker, settlement, || ready(Ok(worker.clone())))
            .await
            .unwrap(),
        receipt
    );
    let closed = reopened
        .manage(&actor, request, &source.latest)
        .await
        .unwrap();
    assert_eq!(
        closed.outcome,
        Some(ManagementOutcome::Applied {
            state: RunState::Paused
        })
    );
    assert_eq!(
        reopened
            .management_receipt(app, &request.request_id)
            .await
            .unwrap(),
        Some(closed)
    );
    let changed = Settlement {
        outcome: JobOutcome::Management {
            outcome: ManagementOutcome::NotFound {},
        },
        ..settlement.clone()
    };
    assert_eq!(
        reopened
            .settle_job(worker, &changed, || ready(Ok(worker.clone())))
            .await,
        Err(Error::Conflict)
    );
}

async fn assert_ready(database: &Database, spec: &JobSpec) {
    let stored = row(
        database,
        "jobs",
        value!({"id":spec.id.as_str(),"app_id":spec.app_id.as_str()}),
    )
    .await;
    assert_eq!(stored["state"], value!("ready"));
    assert_eq!(stored["attempt"], value!(0));
    assert!(stored["worker_id"].is_null());
    assert!(stored["assignment_revision"].is_null());
    assert!(stored["lease_deadline"].is_null());
}

#[expect(
    clippy::too_many_lines,
    reason = "authority revocation and recovery share the same leased job"
)]
async fn claim_authority(fixture: &Fixture) {
    let (coordinator, queue) = host(fixture, Options::default()).await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 1).await;
    let app = AppId::mint();
    let original = place(&coordinator, &app).await;
    let spec = JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    };
    // Placement registration and queue publication share the same scope.
    assert_eq!(queue.submit(&spec).await.unwrap(), spec);
    let database = fixture.database().await;
    let forged = Assignment {
        revision: (original.revision.get() + 1).try_into().unwrap(),
        ..original.clone()
    };
    assert!(matches!(
        coordinator
            .claim_job(&forged.worker_id, &scope(&forged), support::delivery_ceiling(), || std::future::ready(
                Ok(forged.worker_id.clone())
            ))
            .await,
        Err(Error::Denied)
    ));
    assert_ready(&database, &spec).await;
    // The worker gives the placement up; the next visit places the app again
    // under a higher revision, which retires the original authority.
    relinquish(&coordinator, &original).await;
    let replacement = place(&coordinator, &app).await;
    assert!(replacement.revision > original.revision);
    assert!(matches!(
        coordinator
            .claim_job(&original.worker_id, &scope(&original), support::delivery_ceiling(), || {
                std::future::ready(Ok(original.worker_id.clone()))
            })
            .await,
        Err(Error::Denied)
    ));
    assert_ready(&database, &spec).await;
    let foreign_worker = Assignment {
        worker_id: WorkerId::mint(),
        ..replacement.clone()
    };
    assert!(matches!(
        coordinator
            .claim_job(&foreign_worker.worker_id, &scope(&foreign_worker), support::delivery_ceiling(), || {
                std::future::ready(Ok(foreign_worker.worker_id.clone()))
            })
            .await,
        Err(Error::Denied)
    ));
    assert_ready(&database, &spec).await;
    let foreign = AppId::mint();
    queue.register_scope(&foreign).await.unwrap();
    let foreign_scope = Assignment {
        app_id: foreign,
        ..replacement.clone()
    };
    assert!(matches!(
        coordinator
            .claim_job(&foreign_scope.worker_id, &scope(&foreign_scope), support::delivery_ceiling(), || {
                std::future::ready(Ok(foreign_scope.worker_id.clone()))
            })
            .await,
        Err(Error::Denied)
    ));
    assert_ready(&database, &spec).await;

    let assignment_filter = value!({"app_id":app.as_str(),"worker_id":worker.as_str()});
    update(
        &database,
        "assignments",
        assignment_filter.clone(),
        value!({"expires_at":0}),
    )
    .await;
    assert!(matches!(
        coordinator
            .claim_job(&replacement.worker_id, &scope(&replacement), support::delivery_ceiling(), || {
                std::future::ready(Ok(replacement.worker_id.clone()))
            })
            .await,
        Err(Error::Denied)
    ));
    assert_ready(&database, &spec).await;
    relinquish(&coordinator, &replacement).await;
    let current = place(&coordinator, &app).await;
    update(
        &database,
        "workers",
        value!({"id":worker.as_str()}),
        value!({"expires_at":0}),
    )
    .await;
    assert!(matches!(
        coordinator
            .claim_job(&current.worker_id, &scope(&current), support::delivery_ceiling(), || std::future::ready(
                Ok(current.worker_id.clone())
            ))
            .await,
        Err(Error::Denied)
    ));
    assert_ready(&database, &spec).await;
    register(&coordinator, &worker, 1).await;
    let stored_expiry = current.expires_at.get() - 1;
    update(
        &database,
        "assignments",
        assignment_filter,
        value!({"expires_at":stored_expiry}),
    )
    .await;
    let delivery = coordinator
        .claim_job(&current.worker_id, &scope(&current), support::delivery_ceiling(), || {
            std::future::ready(Ok(current.worker_id.clone()))
        })
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(delivery.job, spec);
    assert_eq!(delivery.worker_id, worker);
    assert_eq!(delivery.assignment_revision, current.revision);
    assert_eq!(delivery.attempt.get(), 1);
    assert!(delivery.deadline.get() <= stored_expiry);
    assert!(coordinator
        .claim_job(&current.worker_id, &scope(&current), support::delivery_ceiling(), || std::future::ready(
            Ok(current.worker_id.clone())
        ))
        .await
        .unwrap()
        .is_none());
    let leased = row(&database, "jobs", value!({"id":spec.id.as_str()})).await;
    assert_eq!(leased["state"], value!("leased"));
    assert_eq!(leased["attempt"], value!(1));
    assert_eq!(leased["worker_id"], value!(worker.as_str()));
    assert_eq!(
        leased["assignment_revision"],
        value!(current.revision.get())
    );
}

case!(
    sqlite_worker_publication_restricts_manager_operations,
    postgres_worker_publication_restricts_manager_operations,
    worker_publication
);
case!(
    sqlite_delivery_revalidates_enrollment_and_replays_after_reassignment,
    postgres_delivery_revalidates_enrollment_and_replays_after_reassignment,
    delivery_enrollment
);

fn job(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    }
}

fn publication(assignment: &Assignment, job: JobSpec) -> SubmitJob {
    SubmitJob {
        scope: scope(assignment),
        job,
    }
}

fn manager_operations() -> [JobOperation; 4] {
    [
        JobOperation::Activate {
            deployment_id: DeploymentId::mint(),
            revision: 1.try_into().unwrap(),
        },
        // Retention is the manager's decision; a worker cannot ask itself to
        // give a deployment back.
        JobOperation::ReleaseHold {
            deployment_id: DeploymentId::mint(),
        },
        JobOperation::Cron {
            deployment_id: DeploymentId::mint(),
            schedule_id: ScheduleId::mint(),
            schedule_name: "daily-report".into(),
            request_id: RequestId::mint(),
            run_id: RunId::mint(),
            revision: 1.try_into().unwrap(),
            scheduled_at: 0.try_into().unwrap(),
        },
        JobOperation::Management {
            request_id: RequestId::mint(),
            run_id: RunId::mint(),
            revision: 1.try_into().unwrap(),
            command: zeroship_core::workflow_jobs::ManagementCommand::Transition {
                operation: RunOperation::Pause,
            },
        },
    ]
}

#[expect(
    clippy::too_many_lines,
    reason = "publication rejection controls share the same scoped queue"
)]
async fn worker_publication(fixture: &Fixture) {
    let (coordinator, queue) = host(fixture, Options::default()).await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 1).await;
    let assigned = place(&coordinator, &AppId::mint()).await;
    let database = fixture.database().await;
    for operation in manager_operations() {
        let mut spec = job(&assigned.app_id);
        spec.operation = operation;
        let request = publication(&assigned, spec.clone());
        assert_eq!(
            coordinator
                .submit_job(&worker, &request, || ready(Ok(worker.clone())))
                .await,
            Err(Error::Denied)
        );
        assert!(rows(&database, "jobs", value!({"id":spec.id.as_str()}))
            .await
            .is_empty());
    }
    let foreign = AppId::mint();
    queue.register_scope(&foreign).await.unwrap();
    let spec = job(&assigned.app_id);
    for request in [
        publication(&assigned, job(&foreign)),
        SubmitJob {
            scope: AssignedScope {
                app_id: foreign,
                ..scope(&assigned)
            },
            job: spec.clone(),
        },
        SubmitJob {
            scope: AssignedScope {
                assignment_revision: (assigned.revision.get() + 1).try_into().unwrap(),
                ..scope(&assigned)
            },
            job: spec.clone(),
        },
    ] {
        assert_eq!(
            coordinator
                .submit_job(&worker, &request, || ready(Ok(worker.clone())))
                .await,
            Err(Error::Denied)
        );
        assert!(
            rows(&database, "jobs", value!({"id":request.job.id.as_str()}))
                .await
                .is_empty()
        );
    }
    let request = publication(&assigned, spec.clone());
    assert_eq!(
        coordinator
            .submit_job(&worker, &request, || ready(Ok(WorkerId::mint())))
            .await,
        Err(Error::Denied)
    );
    for _ in 0..2 {
        assert_eq!(
            coordinator
                .submit_job(&worker, &request, || ready(Ok(worker.clone())))
                .await
                .unwrap(),
            spec
        );
    }
    assert_eq!(
        rows(
            &database,
            "jobs",
            value!({"app_id":assigned.app_id.as_str()})
        )
        .await
        .len(),
        1
    );
    let grant = coordinator
        .claim_job(&worker, &scope(&assigned), support::delivery_ceiling(), || ready(Ok(worker.clone())))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&grant.lease().unwrap().delivery, grant.delivery());
    for operation in manager_operations() {
        let mut successor = job(&assigned.app_id);
        successor.operation = operation;
        let command = Settlement {
            delivery: grant.delivery().clone(),
            outcome: JobOutcome::Completed {},
            successors: vec![successor.clone()],
        };
        assert_eq!(
            coordinator
                .settle_job(&worker, &command, || ready(Ok(worker.clone())))
                .await,
            Err(Error::Denied)
        );
        assert_eq!(
            row(&database, "jobs", value!({"id":spec.id.as_str()})).await["state"],
            value!("leased")
        );
        assert!(
            rows(&database, "jobs", value!({"id":successor.id.as_str()}))
                .await
                .is_empty()
        );
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "enrollment failures and receipt replay share one delivery"
)]
async fn delivery_enrollment(fixture: &Fixture) {
    let (coordinator, _) = host(fixture, Options::default()).await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 1).await;
    let assigned = place(&coordinator, &AppId::mint()).await;
    let database = fixture.database().await;
    let spec = job(&assigned.app_id);
    let request = publication(&assigned, spec.clone());
    let checks = Cell::new(0);
    let enrollment = || {
        checks.set(checks.get() + 1);
        ready(if checks.get() == 2 {
            Err(Error::Denied)
        } else {
            Ok(worker.clone())
        })
    };
    assert_eq!(
        coordinator.submit_job(&worker, &request, enrollment).await,
        Err(Error::Denied)
    );
    assert_eq!(checks.get(), 2);
    assert!(rows(&database, "jobs", value!({"id":spec.id.as_str()}))
        .await
        .is_empty());
    coordinator
        .submit_job(&worker, &request, || ready(Ok(worker.clone())))
        .await
        .unwrap();
    checks.set(0);
    assert!(matches!(
        coordinator
            .claim_job(&worker, &scope(&assigned), support::delivery_ceiling(), enrollment)
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(checks.get(), 2);
    assert_ready(&database, &spec).await;

    // The wire selector carries identity only; renewal of the stored placement
    // may extend authority even when an old in-memory snapshot has expired.
    let stale = Assignment {
        expires_at: 0.try_into().unwrap(),
        ..assigned.clone()
    };
    let grant = coordinator
        .claim_job(&worker, &scope(&stale), support::delivery_ceiling(), || ready(Ok(worker.clone())))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(grant.delivery().job, spec);
    assert!(matches!(
        coordinator
            .heartbeat_job(&WorkerId::mint(), grant.delivery(), || ready(Ok(
                worker.clone()
            )))
            .await,
        Err(Error::Denied)
    ));
    let mut foreign_delivery = grant.delivery().clone();
    foreign_delivery.worker_id = WorkerId::mint();
    assert!(matches!(
        coordinator
            .heartbeat_job(&worker, &foreign_delivery, || ready(Ok(worker.clone())))
            .await,
        Err(Error::Denied)
    ));
    let before = row(&database, "jobs", value!({"id":spec.id.as_str()})).await;
    checks.set(0);
    assert!(matches!(
        coordinator
            .heartbeat_job(&worker, grant.delivery(), enrollment)
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(checks.get(), 2);
    assert_eq!(
        row(&database, "jobs", value!({"id":spec.id.as_str()})).await,
        before
    );
    let renewed = coordinator
        .heartbeat_job(&worker, grant.delivery(), || ready(Ok(worker.clone())))
        .await
        .unwrap();
    assert_eq!(renewed.delivery().attempt, grant.delivery().attempt);
    assert!(renewed.delivery().deadline >= grant.delivery().deadline);
    let successor = job(&assigned.app_id);
    let command = Settlement {
        delivery: renewed.delivery().clone(),
        outcome: JobOutcome::Completed {},
        successors: vec![successor.clone()],
    };
    checks.set(0);
    assert_eq!(
        coordinator.settle_job(&worker, &command, enrollment).await,
        Err(Error::Denied)
    );
    assert_eq!(checks.get(), 2);
    assert_eq!(
        row(&database, "jobs", value!({"id":spec.id.as_str()})).await["state"],
        value!("leased")
    );
    assert!(
        rows(&database, "jobs", value!({"id":successor.id.as_str()}))
            .await
            .is_empty()
    );
    let receipt = coordinator
        .settle_job(&worker, &command, || ready(Ok(worker.clone())))
        .await
        .unwrap();
    update(
        &database,
        "assignments",
        value!({"app_id":assigned.app_id.as_str()}),
        value!({"expires_at":0}),
    )
    .await;
    assert_eq!(
        coordinator
            .settle_job(&worker, &command, || ready(Ok(worker.clone())))
            .await
            .unwrap(),
        receipt
    );
    let replacement_worker = WorkerId::mint();
    register(&coordinator, &replacement_worker, 1).await;
    // The first instance gives the app up and drains, so only the replacement
    // remains an eligible candidate.
    relinquish(&coordinator, &assigned).await;
    coordinator
        .register(
            &worker,
            &RegisterWorker {
                capacity: NonZeroU32::new(1).unwrap(),
                state: WorkerState::Draining,
            },
        )
        .await
        .unwrap();
    let replacement = place(&coordinator, &assigned.app_id).await;
    assert_eq!(replacement.worker_id, replacement_worker);
    update(
        &database,
        "workers",
        value!({"id":worker.as_str()}),
        value!({"expires_at":0}),
    )
    .await;
    assert_eq!(
        coordinator
            .settle_job(&worker, &command, || ready(Ok(worker.clone())))
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        coordinator
            .settle_job(&worker, &command, || ready(Err(Error::Denied)))
            .await,
        Err(Error::Denied)
    );
    assert_eq!(
        coordinator
            .settle_job(&worker, &command, || ready(Ok(replacement_worker.clone())))
            .await,
        Err(Error::Denied)
    );
    assert_eq!(
        coordinator
            .settle_job(&replacement_worker, &command, || ready(Ok(
                replacement_worker.clone()
            )))
            .await,
        Err(Error::Denied)
    );
    let mut changed = command.clone();
    changed.outcome = JobOutcome::Waiting {};
    assert_eq!(
        coordinator
            .settle_job(&worker, &changed, || ready(Ok(worker.clone())))
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(
        rows(&database, "jobs", value!({"id":successor.id.as_str()}))
            .await
            .len(),
        1
    );
}

#[compio::test]
async fn postgres_job_enrollment_is_checked_after_waiting_for_scope_lock() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let (coordinator, _) = host(&fixture, Options::default()).await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 1).await;
    let assigned = place(&coordinator, &AppId::mint()).await;
    let request = publication(&assigned, job(&assigned.app_id));
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    admin.batch_execute("BEGIN").await.unwrap();
    assert_eq!(
        admin
            .query(
                "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
                &[&assigned.app_id.as_str()]
            )
            .await
            .unwrap()
            .len(),
        1
    );
    let revoked = Cell::new(false);
    let checks = Cell::new(0);
    let release = async {
        compio::time::timeout(Duration::from_secs(3), async {
            loop {
                admin.query_one("SELECT pg_stat_clear_snapshot()", &[]).await.unwrap();
                let blocked: bool = admin.query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE a.usename='workflow_manager_test' AND NOT l.granted AND l.locktype='transactionid' AND a.query LIKE '%queue_scopes%' AND pg_backend_pid()=ANY(pg_blocking_pids(a.pid)))", &[],
                ).await.unwrap().get(0);
                if blocked { break; }
                compio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("job publication must reach the locked manager scope");
        assert_eq!(checks.get(), 0);
        revoked.set(true);
        admin.batch_execute("ROLLBACK").await.unwrap();
    };
    let publication = coordinator.submit_job(&worker, &request, || {
        checks.set(checks.get() + 1);
        assert!(
            revoked.get(),
            "enrollment must be loaded after the scope wait"
        );
        ready(Err(Error::Denied))
    });
    let (result, ()) = futures::join!(publication, release);
    assert_eq!(result, Err(Error::Denied));
    assert_eq!(checks.get(), 1);
    assert!(rows(
        &fixture.database().await,
        "jobs",
        value!({"id":request.job.id.as_str()})
    )
    .await
    .is_empty());
    coordinator
        .submit_job(&worker, &request, || ready(Ok(worker.clone())))
        .await
        .unwrap();
}

case!(
    sqlite_worker_publishes_scoped_fanout_and_successors,
    postgres_worker_publishes_scoped_fanout_and_successors,
    fanout_publication
);

async fn fanout_publication(fixture: &Fixture) {
    let broadcast = BroadcastId::mint();
    let page = |revision: i64| JobOperation::Fanout {
        broadcast_id: broadcast.clone(),
        revision: revision.try_into().unwrap(),
    };
    let foreign = JobOperation::Fanout {
        broadcast_id: BroadcastId::mint(),
        revision: 1.try_into().unwrap(),
    };
    journal_pages(fixture, "fanout", [page(1), page(2)], [foreign, page(2)]).await;
}

case!(
    sqlite_worker_publishes_scoped_propagation_and_successors,
    postgres_worker_publishes_scoped_propagation_and_successors,
    propagation_publication
);

async fn propagation_publication(fixture: &Fixture) {
    let obligation = PropagationId::mint();
    let page = |revision: i64| JobOperation::Propagate {
        propagation_id: obligation.clone(),
        revision: revision.try_into().unwrap(),
    };
    let foreign = JobOperation::Propagate {
        propagation_id: PropagationId::mint(),
        revision: 1.try_into().unwrap(),
    };
    journal_pages(fixture, "propagate", [page(1), page(2)], [foreign, page(2)]).await;
}

/// A code-free page operation is worker-published, delivered and settled with
/// its successor page, without holds or executable and run projections.
async fn journal_pages(
    fixture: &Fixture,
    kind: &str,
    [first, next]: [JobOperation; 2],
    substitutes: [JobOperation; 2],
) {
    let (coordinator, _) = host(fixture, Options::default()).await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 1).await;
    let assigned = place(&coordinator, &AppId::mint()).await;
    let spec = JobSpec {
        operation: first,
        ..job(&assigned.app_id)
    };
    assert_publication_identity(&coordinator, &worker, &assigned, &spec, substitutes).await;
    let granted = coordinator
        .claim_job(&worker, &scope(&assigned), support::delivery_ceiling(), || ready(Ok(worker.clone())))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(granted.delivery().job, spec);
    let successor = JobSpec {
        id: JobId::mint(),
        operation: next,
        ..spec.clone()
    };
    let mut settlement = Settlement {
        delivery: granted.delivery().clone(),
        outcome: JobOutcome::Management {
            outcome: ManagementOutcome::Denied {},
        },
        successors: vec![successor.clone()],
    };
    let db = fixture.database().await;
    let before = rows(&db, "jobs", value!({"app_id":assigned.app_id.as_str()})).await;
    assert_eq!(
        coordinator
            .settle_job(&worker, &settlement, || ready(Ok(worker.clone())))
            .await,
        Err(Error::Invalid)
    );
    assert_eq!(
        rows(&db, "jobs", value!({"app_id":assigned.app_id.as_str()})).await,
        before
    );
    settlement.outcome = JobOutcome::Waiting {};
    let receipt = coordinator
        .settle_job(&worker, &settlement, || ready(Ok(worker.clone())))
        .await
        .unwrap();
    assert_eq!(
        coordinator
            .settle_job(&worker, &settlement, || ready(Ok(worker.clone())))
            .await
            .unwrap(),
        receipt
    );
    let stored = row(&db, "jobs", value!({"id":successor.id.as_str()})).await;
    assert_eq!(stored["operation_kind"], value!(kind));
    assert!(
        stored["deployment_id"].is_null()
            && stored["run_id"].is_null()
            && stored["management_request_id"].is_null()
    );
    assert!(rows(
        &db,
        "deployment_holds",
        value!({"app_id":assigned.app_id.as_str()})
    )
    .await
    .is_empty());
}

async fn assert_publication_identity(
    coordinator: &Coordinator,
    worker: &WorkerId,
    assigned: &Assignment,
    spec: &JobSpec,
    substitutes: [JobOperation; 2],
) {
    let request = publication(assigned, spec.clone());
    for _ in 0..2 {
        assert_eq!(
            coordinator
                .submit_job(worker, &request, || ready(Ok(worker.clone())))
                .await
                .unwrap(),
            *spec
        );
    }
    for operation in substitutes {
        let changed = publication(
            assigned,
            JobSpec {
                operation,
                ..spec.clone()
            },
        );
        assert_eq!(
            coordinator
                .submit_job(worker, &changed, || ready(Ok(worker.clone())))
                .await,
            Err(Error::Conflict)
        );
    }
    let foreign = publication(
        assigned,
        JobSpec {
            app_id: AppId::mint(),
            ..spec.clone()
        },
    );
    assert_eq!(
        coordinator
            .submit_job(worker, &foreign, || ready(Ok(worker.clone())))
            .await,
        Err(Error::Denied)
    );
}
