#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native fixtures stay on their compio runtime"
)]

#[allow(
    dead_code,
    reason = "shared queue fixtures also expose backend administration"
)]
mod support;

use std::{num::NonZeroU32, time::Duration};
use support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME},
    workflow_coordination::{
        AcknowledgeManagement, AssignScope, AssignedScope, Assignment, ManageRun,
        ManagementOperation, ManagementOutcome, RegisterWorker, RequestId, RunId, RunOperation,
        RunState, WorkerId, WorkerState,
    },
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobSpec},
};
use zeroship_data_orm::{
    orm::{Database, Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    coordinator::{Coordinator, Options},
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
    sqlite_recovery_pages_skip_owned_scopes_without_losing_work,
    postgres_recovery_pages_skip_owned_scopes_without_losing_work,
    recovery_pages
);
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
    let queue = Queue::connect(fixture.binding(), fixture.url(), QueueOptions::default())
        .await
        .unwrap();
    let coordinator = Coordinator::new(queue.clone(), options).unwrap();
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

fn placement(app: &AppId, worker: &WorkerId) -> AssignScope {
    AssignScope {
        request_id: RequestId::mint(),
        app_id: app.clone(),
        worker_id: worker.clone(),
        expected_revision: None,
    }
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

async fn recovery_pages(fixture: &Fixture) {
    let (coordinator, queue) = host(
        fixture,
        Options {
            batch_limit: 1,
            ..Options::default()
        },
    )
    .await;
    let worker = WorkerId::mint();
    register(&coordinator, &worker, 3).await;
    let mut apps: Vec<_> = (0..5).map(|_| AppId::mint()).collect();
    apps.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    for app in &apps {
        queue.register_scope(app).await.unwrap();
    }
    for index in [0, 1, 3] {
        coordinator
            .assign(&placement(&apps[index], &worker))
            .await
            .unwrap();
    }
    // Filtering a source page after its limit would incorrectly stop here.
    assert_eq!(
        coordinator.recovery_scopes(None).await.unwrap(),
        vec![apps[2].clone()]
    );
    assert_eq!(
        coordinator.recovery_scopes(Some(&apps[2])).await.unwrap(),
        vec![apps[4].clone()]
    );
    assert!(coordinator
        .recovery_scopes(Some(&apps[4]))
        .await
        .unwrap()
        .is_empty());
    let placements = coordinator.assignments(&worker, None).await.unwrap();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].app_id, apps[0]);
}

async fn competing_assignments(fixture: &Fixture) {
    let (left, queue) = host(fixture, Options::default()).await;
    let (right, _) = host(fixture, Options::default()).await;
    let worker = WorkerId::mint();
    register(&left, &worker, 1).await;
    let first = placement(&AppId::mint(), &worker);
    let second = placement(&AppId::mint(), &worker);
    queue.register_scope(&first.app_id).await.unwrap();
    queue.register_scope(&second.app_id).await.unwrap();
    let (first_result, second_result) = futures::join!(left.assign(&first), right.assign(&second));
    let (accepted, rejected, assignment) = match (first_result, second_result) {
        (Ok(assignment), Err(Error::Capacity)) => (&first, &second, assignment),
        (Err(Error::Capacity), Ok(assignment)) => (&second, &first, assignment),
        results => panic!("competing placements must obey capacity: {results:?}"),
    };
    assert_eq!(right.assign(accepted).await.unwrap(), assignment);
    assert_eq!(left.assign(rejected).await, Err(Error::Capacity));
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
        let assignment = hosts[0]
            .assign(&placement(&AppId::mint(), &worker))
            .await
            .unwrap();
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
        assert_eq!(
            hosts[2].assign(&placement(&AppId::mint(), &worker)).await,
            Err(Error::Capacity)
        );
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
    let registrations =
        futures::future::join_all(hosts.iter().map(|host| host.register(worker, &request)));
    let results = if let Admin::Postgres(admin) = &fixture.admin {
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
    };
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

#[expect(
    clippy::too_many_lines,
    reason = "the scenario follows receipt identity through placement and lifecycle changes"
)]
async fn management_receipts(fixture: &Fixture) {
    let (coordinator, _) = host(
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
    let assignment = coordinator.assign(&placement(&app, &worker)).await.unwrap();
    let foreign_assignment = coordinator
        .assign(&placement(&foreign, &worker))
        .await
        .unwrap();
    let database = fixture.database().await;
    let assignment_filter = value!({"app_id":app.as_str(),"worker_id":worker.as_str()});
    let original = row(&database, "assignments", assignment_filter.clone()).await;
    register(&coordinator, &worker, 2).await;
    let renewed = coordinator
        .renew(&worker, &scope(&assignment))
        .await
        .unwrap();
    assert_eq!(renewed.revision, assignment.revision);
    assert!(renewed.expires_at >= assignment.expires_at);
    assert_eq!(
        row(&database, "assignments", assignment_filter).await["id"],
        original["id"]
    );
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let request = command(&app);
    assert_eq!(
        coordinator
            .manage(&service_issuer(WORKER_SERVICE_NAME).unwrap(), &request)
            .await,
        Err(Error::Denied)
    );
    let pending = coordinator.manage(&actor, &request).await.unwrap();
    assert_eq!(pending.app_id, app);
    assert_eq!(pending.request_id, request.request_id);
    assert_eq!(pending.outcome, None);
    assert_eq!(coordinator.manage(&actor, &request).await.unwrap(), pending);
    assert_eq!(
        coordinator.manage(&actor, &command(&app)).await,
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
    let management_filter =
        value!({"app_id":app.as_str(),"request_id":request.request_id.as_str()});
    let stored = row(&database, "management", management_filter.clone()).await;
    assert_eq!(
        coordinator
            .pending_management(&worker, &scope(&renewed))
            .await
            .unwrap(),
        vec![request.clone()]
    );
    assert!(coordinator
        .pending_management(&worker, &scope(&foreign_assignment))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        coordinator
            .management_receipt(&foreign, &request.request_id)
            .await
            .unwrap(),
        None
    );
    let ack = AcknowledgeManagement {
        request_id: request.request_id.clone(),
        app_id: app.clone(),
        assignment_revision: renewed.revision,
        outcome: ManagementOutcome::Applied {
            state: RunState::Paused,
        },
    };
    assert_eq!(
        coordinator
            .acknowledge_management(&WorkerId::mint(), &ack)
            .await,
        Err(Error::Denied)
    );
    let foreign_ack = AcknowledgeManagement {
        app_id: foreign,
        assignment_revision: foreign_assignment.revision,
        ..ack.clone()
    };
    assert_eq!(
        coordinator
            .acknowledge_management(&worker, &foreign_ack)
            .await,
        Err(Error::Denied)
    );
    let receipt = coordinator
        .acknowledge_management(&worker, &ack)
        .await
        .unwrap();
    assert_eq!(receipt.outcome, Some(ack.outcome));
    assert_eq!(
        row(&database, "management", management_filter).await["id"],
        stored["id"]
    );
    let (reopened, _) = host(fixture, Options::default()).await;
    assert_eq!(
        reopened
            .acknowledge_management(&worker, &ack)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(reopened.manage(&actor, &request).await.unwrap(), receipt);
    assert_eq!(
        reopened
            .management_receipt(&app, &request.request_id)
            .await
            .unwrap(),
        Some(receipt)
    );
    assert!(reopened
        .pending_management(&worker, &scope(&renewed))
        .await
        .unwrap()
        .is_empty());
    let conflicting = AcknowledgeManagement {
        outcome: ManagementOutcome::NotFound {},
        ..ack
    };
    assert_eq!(
        reopened.acknowledge_management(&worker, &conflicting).await,
        Err(Error::Conflict)
    );
    assert!(coordinator
        .manage(&actor, &command(&app))
        .await
        .unwrap()
        .outcome
        .is_none());
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
    let request = placement(&app, &worker);
    let original = coordinator.assign(&request).await.unwrap();
    let spec = JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        deployment_id: DeploymentId::mint(),
        operation: JobOperation::Advance {
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
    assert_eq!(coordinator.claim(&forged).await, Err(Error::Denied));
    assert_ready(&database, &spec).await;
    let replacement = coordinator
        .assign(&AssignScope {
            request_id: RequestId::mint(),
            expected_revision: Some(original.revision),
            ..request.clone()
        })
        .await
        .unwrap();
    assert!(replacement.revision > original.revision);
    assert_eq!(coordinator.claim(&original).await, Err(Error::Denied));
    assert_ready(&database, &spec).await;
    let foreign_worker = Assignment {
        worker_id: WorkerId::mint(),
        ..replacement.clone()
    };
    assert_eq!(coordinator.claim(&foreign_worker).await, Err(Error::Denied));
    assert_ready(&database, &spec).await;
    let foreign = AppId::mint();
    queue.register_scope(&foreign).await.unwrap();
    let foreign_scope = Assignment {
        app_id: foreign,
        ..replacement.clone()
    };
    assert_eq!(coordinator.claim(&foreign_scope).await, Err(Error::Denied));
    assert_ready(&database, &spec).await;

    let assignment_filter = value!({"app_id":app.as_str(),"worker_id":worker.as_str()});
    update(
        &database,
        "assignments",
        assignment_filter.clone(),
        value!({"expires_at":0}),
    )
    .await;
    assert_eq!(coordinator.claim(&replacement).await, Err(Error::Denied));
    assert_ready(&database, &spec).await;
    let current = coordinator
        .assign(&AssignScope {
            request_id: RequestId::mint(),
            expected_revision: Some(replacement.revision),
            ..request
        })
        .await
        .unwrap();
    update(
        &database,
        "workers",
        value!({"id":worker.as_str()}),
        value!({"expires_at":0}),
    )
    .await;
    assert_eq!(coordinator.claim(&current).await, Err(Error::Denied));
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
    let delivery = coordinator.claim(&current).await.unwrap().unwrap();
    assert_eq!(delivery.job, spec);
    assert_eq!(delivery.worker_id, worker);
    assert_eq!(delivery.assignment_revision, current.revision);
    assert_eq!(delivery.attempt.get(), 1);
    assert!(delivery.deadline.get() <= stored_expiry);
    assert_eq!(coordinator.claim(&current).await.unwrap(), None);
    let leased = row(&database, "jobs", value!({"id":spec.id.as_str()})).await;
    assert_eq!(leased["state"], value!("leased"));
    assert_eq!(leased["attempt"], value!(1));
    assert_eq!(leased["worker_id"], value!(worker.as_str()));
    assert_eq!(
        leased["assignment_revision"],
        value!(current.revision.get())
    );
}
