use super::*;
use std::future::ready;
use zeroship_core::{
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    workflow_coordination::{
        ManageRun, ManagementOperation, ManagementOutcome, RequestId, RestartDeploy,
        RestartOptions, RestartTarget, RunOperation, RunState,
    },
    workflow_jobs::{BroadcastId, ManagementCommand, PropagationId},
};
use zeroship_workflow_manager::{
    coordinator::Options as CoordinatorOptions,
    recovery::DutyKind,
    retention::HoldFuture,
};

case!(
    sqlite_journal_jobs_never_call_deployment_holds,
    postgres_journal_jobs_never_call_deployment_holds,
    journal_jobs
);
case!(
    sqlite_queue_rejects_deployment_projection_mismatch,
    postgres_queue_rejects_deployment_projection_mismatch,
    projection_mismatch
);
case!(
    sqlite_pending_recovery_allows_provenance_reclamation,
    postgres_pending_recovery_allows_provenance_reclamation,
    provenance_reclamation
);
case!(
    sqlite_recovery_rejects_pending_executable_job,
    postgres_recovery_rejects_pending_executable_job,
    pending_identity
);
case!(
    sqlite_release_checks_dependencies_beyond_first_page,
    postgres_release_checks_dependencies_beyond_first_page,
    paged_dependencies
);

#[derive(Debug)]
struct ForbiddenHolds;

impl HoldClient for ForbiddenHolds {
    fn acquire<'a>(
        &'a self,
        _: &'a AppId,
        _: &'a DeploymentId,
        _: HoldGeneration,
    ) -> HoldFuture<'a> {
        panic!("journal-only job attempted to acquire code")
    }

    fn release<'a>(
        &'a self,
        _: &'a AppId,
        _: &'a DeploymentId,
        _: HoldGeneration,
    ) -> HoldFuture<'a> {
        panic!("journal-only job attempted to release code")
    }
}

async fn journal_jobs(fixture: &Fixture) {
    let queue = queue(fixture, Rc::new(ForbiddenHolds)).await;
    for operation in [
        JobOperation::Reconcile {},
        JobOperation::Collect {},
        JobOperation::Fanout {
            broadcast_id: BroadcastId::mint(),
            revision: 1.try_into().unwrap(),
        },
        JobOperation::Propagate {
            propagation_id: PropagationId::mint(),
            revision: 1.try_into().unwrap(),
        },
    ] {
        exercise_journal_job(fixture, &queue, operation).await;
    }
    journal_commands(fixture, &queue).await;

    let app = AppId::mint();
    let recovery = Recovery::new(queue.clone(), RecoveryOptions::default()).unwrap();
    let provenance = DeploymentId::mint();
    recovery
        .ensure(&app, &provenance, 1.try_into().unwrap())
        .await
        .unwrap();
    for kind in [DutyKind::Reconcile, DutyKind::Collect] {
        let pending = recovery.dispatch(&app, kind).await.unwrap().unwrap();
        assert_eq!(pending.deployment_id(), None);
        assert_eq!(
            recovery.dispatch(&app, kind).await.unwrap(),
            Some(pending.clone())
        );
        finish(&queue, &assignment(&app), &pending).await;
    }
    assert!(rows(fixture, "deployment_holds", value!({}))
        .await
        .is_empty());
}

async fn exercise_journal_job(fixture: &Fixture, queue: &Queue, operation: JobOperation) {
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(&app);
    let spec = JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation,
        available_at: 0.try_into().unwrap(),
    };
    assert_eq!(spec.deployment_id(), None);
    assert_eq!(queue.submit(&spec).await.unwrap(), spec);
    assert_eq!(queue.submit(&spec).await.unwrap(), spec);
    assert_eq!(
        queue
            .submit_authorized(&(&authority).into(), &spec, |_| ready(
                Ok(authority.clone())
            ))
            .await
            .unwrap(),
        spec
    );
    let granted = queue.claim(&authority).await.unwrap().unwrap();
    assert_eq!(granted.delivery().job, spec);
    let renewed = queue
        .heartbeat(&authority, granted.delivery())
        .await
        .unwrap();
    let successor = JobSpec {
        id: JobId::mint(),
        ..spec.clone()
    };
    let settlement = Settlement {
        delivery: renewed.delivery().clone(),
        outcome: JobOutcome::Completed {},
        successors: vec![successor.clone()],
    };
    let receipt = queue.settle(&authority, &settlement).await.unwrap();
    assert_eq!(
        queue.settle(&authority, &settlement).await.unwrap(),
        receipt
    );
    finish(queue, &authority, &successor).await;
    assert_eq!(queue.submit(&spec).await.unwrap(), spec);
    assert!(queue.claim(&authority).await.unwrap().is_none());
    assert!(
        rows(fixture, "deployment_holds", value!({"app_id":app.as_str()}))
            .await
            .is_empty()
    );
    let stored = rows(fixture, "jobs", value!({"app_id":app.as_str()})).await;
    assert!(!stored.is_empty());
    assert!(stored.iter().all(|row| row["deployment_id"] == Value::Null));
}

async fn journal_commands(fixture: &Fixture, queue: &Queue) {
    let coordinator = support::coordinator(queue, CoordinatorOptions::default());
    let actor = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let mut commands: Vec<_> = [
        RunOperation::Pause,
        RunOperation::Resume,
        RunOperation::Cancel,
    ]
    .into_iter()
    .map(|operation| ManagementOperation::Transition { operation })
    .collect();
    for from in [
        None,
        Some(RestartTarget {
            name: "retained-step".into(),
            occurrence: Some(0),
        }),
    ] {
        commands.push(ManagementOperation::Restart {
            options: RestartOptions {
                from,
                deploy: Some(RestartDeploy::Started),
            },
            deployment: None,
        });
    }
    for command in commands {
        let app = AppId::mint();
        let request = ManageRun {
            app_id: app.clone(),
            request_id: RequestId::mint(),
            run_id: RunId::mint(),
            command,
        };
        let pending = coordinator.manage(&actor, &request).await.unwrap();
        assert_eq!(coordinator.manage(&actor, &request).await.unwrap(), pending);
        let authority = assignment(&app);
        let granted = queue.claim(&authority).await.unwrap().unwrap();
        assert_eq!(granted.delivery().job.deployment_id(), None);
        let renewed = queue
            .heartbeat(&authority, granted.delivery())
            .await
            .unwrap();
        for outcome in [
            JobOutcome::Completed {},
            JobOutcome::Waiting {},
            JobOutcome::Rejected {},
        ] {
            let rejected = Settlement {
                delivery: renewed.delivery().clone(),
                outcome,
                successors: vec![job(&app, &DeploymentId::mint())],
            };
            super::outcomes::refused(fixture, queue, &authority, &rejected, Error::Invalid).await;
        }
        // A management result has to answer the command the enqueued job
        // carries, so the settled outcome is read off that command rather than
        // standing in for every one of them.
        let JobOperation::Management { command, .. } = &renewed.delivery().job.operation else {
            panic!("a management request must enqueue a management job");
        };
        let result = match command {
            ManagementCommand::Transition { .. } => ManagementOutcome::Applied {
                state: RunState::Paused,
            },
            ManagementCommand::RestartStarted { .. } | ManagementCommand::RestartLatest { .. } => {
                ManagementOutcome::Restarted {
                    state: RunState::Queued,
                    restarted_from_ordinal: Some(4),
                    pinned_to: DeploymentId::mint(),
                }
            }
        };
        let settlement = Settlement {
            delivery: renewed.delivery().clone(),
            outcome: JobOutcome::Management { outcome: result },
            successors: vec![],
        };
        let settled = queue.settle(&authority, &settlement).await.unwrap();
        assert_eq!(
            queue.settle(&authority, &settlement).await.unwrap(),
            settled
        );
        assert!(coordinator
            .manage(&actor, &request)
            .await
            .unwrap()
            .outcome
            .is_some());
    }
}

async fn set_projection(fixture: &Fixture, spec: &JobSpec, projection: Value) {
    let output = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .execute(zeroship_data_orm::orm::Operation::Update {
            filter: value!({"app_id":spec.app_id.as_str(),"id":spec.id.as_str()}),
            patch: value!({"deployment_id":projection}),
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(output, Output::Count(1)));
}

async fn set_envelope(fixture: &Fixture, spec: &JobSpec) {
    let output = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .execute(zeroship_data_orm::orm::Operation::Update {
            filter: value!({"app_id":spec.app_id.as_str(),"id":spec.id.as_str()}),
            patch: value!({
                "deployment_id":spec.deployment_id().map(DeploymentId::as_str),
                "operation":serde_json::to_string(&spec.operation).unwrap(),
            }),
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(output, Output::Count(1)));
}

async fn assert_release_refused(
    fixture: &Fixture,
    queue: &Queue,
    faults: &FaultClient,
    app: &AppId,
    deployment: &DeploymentId,
    expected: Error,
) {
    let released = faults.released.get();
    let holds = rows(fixture, "deployment_holds", value!({"app_id":app.as_str()})).await;
    assert_eq!(
        queue.release_deployment(app, deployment).await,
        Err(expected)
    );
    assert_eq!(faults.released.get(), released);
    assert_eq!(
        rows(fixture, "deployment_holds", value!({"app_id":app.as_str()})).await,
        holds
    );
}

async fn reject_ready_projection(
    fixture: &Fixture,
    queue: &Queue,
    faults: &FaultClient,
    authority: &Assignment,
    spec: &JobSpec,
) {
    let app = &spec.app_id;
    let before = rows(fixture, "jobs", value!({"app_id":app.as_str()})).await;
    let scope_before = rows(fixture, "queue_scopes", value!({"id":app.as_str()})).await;
    assert_eq!(queue.submit(spec).await, Err(Error::Storage));
    assert!(matches!(queue.claim(authority).await, Err(Error::Storage)));
    if let Some(deployment) = spec.deployment_id() {
        assert_release_refused(fixture, queue, faults, app, deployment, Error::Storage).await;
    }
    assert_eq!(
        rows(fixture, "jobs", value!({"app_id":app.as_str()})).await,
        before
    );
    assert_eq!(
        rows(fixture, "queue_scopes", value!({"id":app.as_str()})).await,
        scope_before
    );
}

async fn reject_leased_projection(
    fixture: &Fixture,
    queue: &Queue,
    faults: &FaultClient,
    authority: &Assignment,
    settlement: &Settlement,
    projection: Value,
) {
    let spec = &settlement.delivery.job;
    let app = &spec.app_id;
    set_projection(fixture, spec, projection).await;
    let before = rows(fixture, "jobs", value!({"app_id":app.as_str()})).await;
    assert!(matches!(
        queue.heartbeat(authority, &settlement.delivery).await,
        Err(Error::Storage)
    ));
    assert_eq!(
        queue.settle(authority, settlement).await,
        Err(Error::Storage)
    );
    if let Some(deployment) = spec.deployment_id() {
        assert_release_refused(fixture, queue, faults, app, deployment, Error::Storage).await;
    }
    assert_eq!(
        rows(fixture, "jobs", value!({"app_id":app.as_str()})).await,
        before
    );
}

async fn projection_mismatch(fixture: &Fixture) {
    let faults = FaultClient::new(support::synthetic_holds());
    let queue = queue(fixture, faults.clone()).await;
    for operation in [
        JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        JobOperation::Reconcile {},
        JobOperation::Fanout {
            broadcast_id: BroadcastId::mint(),
            revision: 1.try_into().unwrap(),
        },
        JobOperation::Propagate {
            propagation_id: PropagationId::mint(),
            revision: 1.try_into().unwrap(),
        },
    ] {
        let app = AppId::mint();
        queue.register_scope(&app).await.unwrap();
        let authority = assignment(&app);
        let spec = JobSpec {
            id: JobId::mint(),
            app_id: app.clone(),
            operation,
            available_at: 0.try_into().unwrap(),
        };
        queue.submit(&spec).await.unwrap();
        let expected = value!(spec.deployment_id().map(DeploymentId::as_str));
        let mut wrong = vec![value!(DeploymentId::mint().as_str()), value!("invalid")];
        if spec.deployment_id().is_some() {
            wrong.push(Value::Null);
        }
        for projection in &wrong {
            set_projection(fixture, &spec, projection.clone()).await;
            reject_ready_projection(fixture, &queue, &faults, &authority, &spec).await;
        }
        let mut changed = spec.clone();
        if let JobOperation::Advance { deployment_id, .. } = &mut changed.operation {
            *deployment_id = DeploymentId::mint();
        } else {
            changed.operation = JobOperation::Collect {};
        }
        set_envelope(fixture, &changed).await;
        reject_ready_projection(fixture, &queue, &faults, &authority, &spec).await;
        set_envelope(fixture, &spec).await;
        if let Some(deployment) = spec.deployment_id() {
            assert_release_refused(fixture, &queue, &faults, &app, deployment, Error::Conflict)
                .await;
        }
        let grant = queue.claim(&authority).await.unwrap().unwrap();
        let settlement = Settlement {
            delivery: grant.delivery().clone(),
            outcome: JobOutcome::Completed {},
            successors: vec![],
        };
        for projection in wrong {
            reject_leased_projection(
                fixture,
                &queue,
                &faults,
                &authority,
                &settlement,
                projection,
            )
            .await;
        }
        set_projection(fixture, &spec, expected).await;
        queue.settle(&authority, &settlement).await.unwrap();
        if let Some(deployment) = spec.deployment_id() {
            queue.release_deployment(&app, deployment).await.unwrap();
        }
    }
}

async fn submit_dependency_after_first_page(
    fixture: &Fixture,
    queue: &Queue,
    app: &AppId,
    deployment: &DeploymentId,
) -> (Vec<JobSpec>, JobSpec) {
    let mut ids: Vec<_> = (0..=256).map(|_| JobId::mint()).collect();
    ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let dependency = JobSpec {
        id: ids.pop().unwrap(),
        ..job(app, deployment)
    };
    let mut pending: Vec<_> = ids
        .into_iter()
        .map(|id| JobSpec {
            id,
            app_id: app.clone(),
            operation: JobOperation::Reconcile {},
            available_at: 0.try_into().unwrap(),
        })
        .collect();
    pending.push(dependency.clone());
    for spec in &pending {
        queue.submit(spec).await.unwrap();
    }
    let Output::Rows {
        rows: first_page, ..
    } = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .find(
            value!({"app_id":app.as_str(),"state":{"$ne":"settled"}}),
            value!({"orderBy":{"id":1},"limit":256}),
        )
        .await
        .unwrap()
    else {
        panic!("expected pending jobs")
    };
    assert_eq!(first_page.len(), pending.len() - 1);
    assert!(first_page
        .iter()
        .all(|row| row["deployment_id"] == Value::Null));
    assert!(first_page
        .iter()
        .all(|row| row["id"].as_str().unwrap() < dependency.id.as_str()));
    (pending, dependency)
}

async fn paged_dependencies(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let faults = FaultClient::new(catalog.client());
    let queue = queue(fixture, faults.clone()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let deployment = catalog.publish(&app, "late queue dependency", &[]).await;
    let (pending, dependency) =
        submit_dependency_after_first_page(fixture, &queue, &app, &deployment.id).await;
    catalog.assert_retained(&app, &deployment).await;
    assert_release_refused(
        fixture,
        &queue,
        &faults,
        &app,
        &deployment.id,
        Error::Conflict,
    )
    .await;

    set_projection(fixture, &dependency, Value::Null).await;
    let damaged = rows(fixture, "jobs", value!({"id":dependency.id.as_str()})).await;
    assert_release_refused(
        fixture,
        &queue,
        &faults,
        &app,
        &deployment.id,
        Error::Storage,
    )
    .await;
    assert_eq!(
        rows(fixture, "jobs", value!({"id":dependency.id.as_str()})).await,
        damaged
    );
    set_projection(fixture, &dependency, value!(deployment.id.as_str())).await;
    assert_release_refused(
        fixture,
        &queue,
        &faults,
        &app,
        &deployment.id,
        Error::Conflict,
    )
    .await;

    let authority = assignment(&app);
    for spec in &pending {
        finish(&queue, &authority, spec).await;
    }
    assert!(queue.claim(&authority).await.unwrap().is_none());
    let released = faults.released.get();
    let receipt = queue
        .release_deployment(&app, &deployment.id)
        .await
        .unwrap();
    assert_eq!(receipt.state, HoldState::Released);
    assert_eq!(faults.released.get(), released + 1);
    catalog.reclaim(&app, &deployment).await;
}

async fn provenance_reclamation(fixture: &Fixture) {
    let catalog = Catalog::new(fixture).await;
    let faults = FaultClient::new(catalog.client());
    let queue = queue(fixture, faults.clone()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let provenance = catalog.publish(&app, "recovery provenance", &[]).await;
    queue.ensure_deployment(&app, &provenance.id).await.unwrap();
    let recovery = Recovery::new(queue.clone(), RecoveryOptions::default()).unwrap();
    recovery
        .ensure(&app, &provenance.id, 1.try_into().unwrap())
        .await
        .unwrap();
    let pending = recovery
        .dispatch(&app, DutyKind::Reconcile)
        .await
        .unwrap()
        .unwrap();
    let responsibility = rows(fixture, "recovery_scopes", value!({"id":app.as_str()})).await;
    assert_eq!(
        responsibility[0]["deployment_id"],
        value!(provenance.id.as_str())
    );
    assert_eq!(pending.deployment_id(), None);
    catalog.assert_retained(&app, &provenance).await;
    queue
        .release_deployment(&app, &provenance.id)
        .await
        .unwrap();
    catalog.reclaim(&app, &provenance).await;
    let acquired = faults.acquired.get();
    let released = faults.released.get();
    recovery
        .ensure(&app, &provenance.id, 1.try_into().unwrap())
        .await
        .unwrap();
    assert_eq!(
        recovery.dispatch(&app, DutyKind::Reconcile).await.unwrap(),
        Some(pending.clone())
    );
    assert_eq!(
        rows(fixture, "recovery_scopes", value!({"id":app.as_str()})).await,
        responsibility
    );
    let authority = assignment(&app);
    let grant = queue.claim(&authority).await.unwrap().unwrap();
    assert_eq!(grant.delivery().job, pending);
    let renewed = queue.heartbeat(&authority, grant.delivery()).await.unwrap();
    queue
        .settle(
            &authority,
            &Settlement {
                delivery: renewed.delivery().clone(),
                outcome: JobOutcome::Waiting {},
                successors: vec![],
            },
        )
        .await
        .unwrap();
    let next = recovery
        .dispatch(&app, DutyKind::Reconcile)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(next.id, pending.id);
    assert_eq!(next.deployment_id(), None);
    assert_eq!(faults.acquired.get(), acquired);
    assert_eq!(faults.released.get(), released);
}

async fn pending_identity(fixture: &Fixture) {
    let queue = queue(fixture, support::synthetic_holds()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let code = job(&app, &DeploymentId::mint());
    queue.submit(&code).await.unwrap();
    let recovery = Recovery::new(queue.clone(), RecoveryOptions::default()).unwrap();
    recovery
        .ensure(&app, code.deployment_id().unwrap(), 1.try_into().unwrap())
        .await
        .unwrap();
    let pending = recovery
        .dispatch(&app, DutyKind::Reconcile)
        .await
        .unwrap()
        .unwrap();
    for settled in [false, true] {
        if settled {
            finish(&queue, &assignment(&app), &code).await;
        }
        let output = fixture
            .database()
            .await
            .collection("recovery_duties")
            .unwrap()
            .execute(zeroship_data_orm::orm::Operation::Update {
                filter: value!({"app_id":app.as_str(),"kind":"reconcile"}),
                patch: value!({"pending_job_id":code.id.as_str()}),
                many: true,
            })
            .await
            .unwrap();
        assert!(matches!(output, Output::Count(1)));
        let scope = rows(
            fixture,
            "recovery_duties",
            value!({"app_id":app.as_str(),"kind":"reconcile"}),
        )
        .await;
        let jobs = rows(fixture, "jobs", value!({"app_id":app.as_str()})).await;
        assert_eq!(
            recovery.dispatch(&app, DutyKind::Reconcile).await,
            Err(Error::Storage)
        );
        assert_eq!(
            rows(
                fixture,
                "recovery_duties",
                value!({"app_id":app.as_str(),"kind":"reconcile"})
            )
            .await,
            scope
        );
        assert_eq!(
            rows(fixture, "jobs", value!({"app_id":app.as_str()})).await,
            jobs
        );
        fixture
            .database()
            .await
            .collection("recovery_duties")
            .unwrap()
            .update(
                value!({"app_id":app.as_str(),"kind":"reconcile"}),
                value!({"pending_job_id":pending.id.as_str()}),
            )
            .await
            .unwrap();
        assert_eq!(
            recovery.dispatch(&app, DutyKind::Reconcile).await.unwrap(),
            Some(pending.clone())
        );
    }
}
