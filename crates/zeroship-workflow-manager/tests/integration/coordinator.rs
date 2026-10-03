#![allow(
    clippy::future_not_send,
    reason = "native fixtures stay on their compio runtime"
)]

use crate::support;

use std::{cell::Cell, future::ready, num::NonZeroU32, time::Duration};
use crate::support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME},
    workflow_coordination::{
        AssignedScope, Assignment, ManageRun, ManagementOperation, ManagementOutcome,
        RegisterWorker, RegisteredWorker, ReleaseReason, ReleaseScope, RequestId, RunId,
        RunOperation, RunState, WorkerId, WorkerState,
    },
    workflow_jobs::{
        BroadcastId, DeploymentId, JobId, JobOperation, JobOutcome, JobReceipt, JobSpec,
        JournalSettlement, PropagationId, SettlementRefusal,
    },
};
use zeroship_data_orm::{
    orm::{Database, Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    coordinator::{Coordinator, Options, Placed},
    maintenance::MaintenanceAuthority,
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
    let (coordinator, queue) = host(
        fixture,
        Options {
            max_pending_management: 1,
            ..Options::default()
        },
    )
    .await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 2).await;
    let app = AppId::mint();
    let foreign = AppId::mint();
    let assignment = place(&coordinator, &app).await;
    let other = place(&coordinator, &foreign).await;
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let request = command(&app);
    accept_management_receipt(&coordinator, &request).await;
    assert_eq!(
        coordinator
            .management_receipt(&foreign, &request.request_id)
            .await
            .unwrap(),
        None
    );
    // A command is a sweep: no placement takes one, the app it names or another.
    for placed in [&other, &assignment] {
        assert!(coordinator
            .claim_job(&worker, &scope(placed), Ok(support::delivery_ceiling()), || ready(Ok(worker.clone())))
            .await
            .unwrap()
            .is_none());
    }
    // The lane of the process that owns the journal takes it, and only for the
    // app it holds: the foreign app's lane has nothing.
    assert!(MaintenanceAuthority::new(foreign.clone(), WorkerId::mint())
        .claim(&queue, Ok(support::delivery_ceiling()))
        .await
        .unwrap()
        .is_none());
    let lane = MaintenanceAuthority::new(app.clone(), WorkerId::mint());
    let delivery = lane
        .claim(&queue, Ok(support::delivery_ceiling()))
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
    let settlement = support::settlement_from(delivery, JobOutcome::Management {
            outcome: ManagementOutcome::Applied {
                state: RunState::Paused,
            },
        });
    replay_management_receipt(fixture, &queue, &lane, &request, &settlement).await;
    assert!(coordinator
        .manage(&actor, &command(&app))
        .await
        .unwrap()
        .outcome
        .is_none());
}

async fn accept_management_receipt(coordinator: &Coordinator, request: &ManageRun) {
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    assert_eq!(
        coordinator
            .manage(&service_issuer(WORKER_SERVICE_NAME).unwrap(), request)
            .await,
        Err(Error::Denied)
    );
    let pending = coordinator.manage(&actor, request).await.unwrap();
    assert_eq!(pending.outcome, None);
    assert_eq!(
        coordinator.manage(&actor, request).await.unwrap(),
        pending
    );
    assert_eq!(
        coordinator.manage(&actor, &command(&request.app_id)).await,
        Err(Error::Capacity)
    );
    let changed = ManageRun {
        run_id: RunId::mint(),
        ..request.clone()
    };
    assert_eq!(
        coordinator.manage(&actor, &changed).await,
        Err(Error::Conflict)
    );
}

async fn replay_management_receipt(
    fixture: &Fixture,
    queue: &Queue,
    lane: &MaintenanceAuthority,
    request: &ManageRun,
    settlement: &JournalSettlement,
) {
    let app = &request.app_id;
    // Another holder of the same lane did not take this delivery, so it cannot
    // discharge it either.
    assert_eq!(
        MaintenanceAuthority::new(app.clone(), WorkerId::mint())
            .settle(queue, settlement)
            .await,
        Err(Error::Denied)
    );
    let receipt = lane.settle(queue, settlement).await.unwrap();
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let (reopened, reopened_queue) = host(fixture, Options::default()).await;
    assert_eq!(
        lane.settle(&reopened_queue, settlement).await.unwrap(),
        receipt
    );
    let closed = reopened.manage(&actor, request).await.unwrap();
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
    let changed = support::settlement_from(settlement.clone().delivery().clone(), JobOutcome::Management {
            outcome: ManagementOutcome::NotFound {},
        });
    assert_eq!(
        lane.settle(&reopened_queue, &changed).await,
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
            .claim_job(&forged.worker_id, &scope(&forged), Ok(support::delivery_ceiling()), || std::future::ready(
                Ok(forged.worker_id.clone())
            ))
            .await,
        Err(Error::Denied)
    ));
    assert_ready(&database, &spec).await;
    // The worker gives the placement up; the next visit places the app again
    // under a higher revision, which retires the original authority. The refusal
    // is a CONFLICT rather than a denial: this instance is still the placed one
    // and the revision it names was superseded, so the next scan reaches the one
    // that holds. The forged revision above stays denied, because nothing ever
    // granted it.
    relinquish(&coordinator, &original).await;
    let replacement = place(&coordinator, &app).await;
    assert!(replacement.revision > original.revision);
    assert!(matches!(
        coordinator
            .claim_job(&original.worker_id, &scope(&original), Ok(support::delivery_ceiling()), || {
                std::future::ready(Ok(original.worker_id.clone()))
            })
            .await,
        Err(Error::Conflict)
    ));
    assert_ready(&database, &spec).await;
    let foreign_worker = Assignment {
        worker_id: WorkerId::mint(),
        ..replacement.clone()
    };
    assert!(matches!(
        coordinator
            .claim_job(&foreign_worker.worker_id, &scope(&foreign_worker), Ok(support::delivery_ceiling()), || {
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
    // This scope is registered, so the refusal comes from placement rather than
    // from the scope lock. The ceiling is the only variable across these claims:
    // a caller whose policy authority could not answer for the app carries that
    // failure in, and one whose authority answered with a ceiling no claim could
    // satisfy carries that. Either way the refusal must still name the placement,
    // because answering with the caller's own unavailability or an invalid budget
    // would tell a worker to retry a scope it can never hold. The readable,
    // satisfiable ceiling is the control.
    for ceiling in [
        Err(Error::Unavailable),
        Err(Error::Invalid),
        Ok(0),
        Ok(support::delivery_ceiling()),
    ] {
        assert_eq!(
            coordinator
                .claim_job(&foreign_scope.worker_id, &scope(&foreign_scope), ceiling, || {
                    std::future::ready(Ok(foreign_scope.worker_id.clone()))
                })
                .await
                .err(),
            Some(Error::Denied)
        );
        assert_ready(&database, &spec).await;
    }
    // The same unreadable ceiling under held placement is the caller's own
    // failure, so it surfaces once nothing else refuses the claim first.
    assert_eq!(
        coordinator
            .claim_job(&replacement.worker_id, &scope(&replacement), Err(Error::Unavailable), || {
                std::future::ready(Ok(replacement.worker_id.clone()))
            })
            .await
            .err(),
        Some(Error::Unavailable)
    );
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
            .claim_job(&replacement.worker_id, &scope(&replacement), Ok(support::delivery_ceiling()), || {
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
            .claim_job(&current.worker_id, &scope(&current), Ok(support::delivery_ceiling()), || std::future::ready(
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
        .claim_job(&current.worker_id, &scope(&current), Ok(support::delivery_ceiling()), || {
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
        .claim_job(&current.worker_id, &scope(&current), Ok(support::delivery_ceiling()), || std::future::ready(
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

#[expect(
    clippy::too_many_lines,
    reason = "enrollment failures and receipt replay share one delivery"
)]
async fn delivery_enrollment(fixture: &Fixture) {
    let (coordinator, queue) = host(fixture, Options::default()).await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 1).await;
    let assigned = place(&coordinator, &AppId::mint()).await;
    let database = fixture.database().await;
    let spec = job(&assigned.app_id);
    queue.submit(&spec).await.unwrap();
    let checks = Cell::new(0);
    let enrollment = || {
        checks.set(checks.get() + 1);
        ready(if checks.get() == 2 {
            Err(Error::Denied)
        } else {
            Ok(worker.clone())
        })
    };
    checks.set(0);
    assert!(matches!(
        coordinator
            .claim_job(&worker, &scope(&assigned), Ok(support::delivery_ceiling()), enrollment)
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
        .claim_job(&worker, &scope(&stale), Ok(support::delivery_ceiling()), || ready(Ok(worker.clone())))
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
    let command = support::settlement_from(renewed.delivery().clone(), JobOutcome::Completed {});
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
    let changed = support::settlement_from(command.delivery().clone(), JobOutcome::Waiting {});
    assert_eq!(
        coordinator
            .settle_job(&worker, &changed, || ready(Ok(worker.clone())))
            .await,
        Err(Error::Conflict)
    );
}

case!(
    sqlite_worker_publishes_scoped_fanout,
    postgres_worker_publishes_scoped_fanout,
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
    sqlite_worker_publishes_scoped_propagation,
    postgres_worker_publishes_scoped_propagation,
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

/// A code-free page operation is published by the service, then delivered to the
/// lane and settled with its own scheduling outcome, without holds or executable
/// and run projections.
///
/// Publication and delivery part company here: the page is published as a
/// creator intent the journal produced, and the worker that holds the placement
/// never runs it, because the page is a sweep of the journal. A page publishes
/// its next page through the journal's frontier, never through the queue's
/// settlement.
async fn journal_pages(
    fixture: &Fixture,
    _kind: &str,
    [first, _next]: [JobOperation; 2],
    substitutes: [JobOperation; 2],
) {
    let (coordinator, queue) = host(fixture, Options::default()).await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 1).await;
    let assigned = place(&coordinator, &AppId::mint()).await;
    let spec = JobSpec {
        operation: first,
        ..job(&assigned.app_id)
    };
    assert_publication_identity(&queue, &spec, substitutes).await;
    assert!(
        coordinator
            .claim_job(&worker, &scope(&assigned), Ok(support::delivery_ceiling()), || ready(Ok(worker.clone())))
            .await
            .unwrap()
            .is_none(),
        "the worker that published the page is not the host that runs it"
    );
    let lane = MaintenanceAuthority::new(assigned.app_id.clone(), WorkerId::mint());
    let granted = lane
        .claim(&queue, Ok(support::delivery_ceiling()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(granted.delivery().job, spec);
    let db = fixture.database().await;
    // A management result does not answer a page, so it cannot become a
    // settlement at all.
    assert_eq!(
        JournalSettlement::from_receipt(
            &JobReceipt {
                job: granted.delivery().job.clone(),
                outcome: JobOutcome::Management {
                    outcome: ManagementOutcome::Denied {},
                },
            },
            granted.delivery(),
        )
        .unwrap_err(),
        SettlementRefusal::Invalid
    );
    let settlement = support::settlement(granted.delivery(), JobOutcome::Waiting {});
    let receipt = lane.settle(&queue, &settlement).await.unwrap();
    assert_eq!(lane.settle(&queue, &settlement).await.unwrap(), receipt);
    assert!(rows(
        &db,
        "deployment_holds",
        value!({"app_id":assigned.app_id.as_str()})
    )
    .await
    .is_empty());
}

async fn assert_publication_identity(
    queue: &Queue,
    spec: &JobSpec,
    substitutes: [JobOperation; 2],
) {
    for _ in 0..2 {
        assert_eq!(queue.submit(spec).await.unwrap(), *spec);
    }
    for operation in substitutes {
        let changed = JobSpec {
            operation,
            ..spec.clone()
        };
        assert_eq!(queue.submit(&changed).await, Err(Error::Conflict));
    }
}
