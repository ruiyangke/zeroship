#![expect(clippy::future_not_send, reason = "native delivery tests use compio")]

use super::{
    publication::{Manager, Publisher},
    *,
};
use crate::service::{
    delivery::{DeliveredTask, JobAcceptance},
    AppWorkflows, WorkerIdentity,
};
use std::time::{Duration, Instant};
use zeroship_core::{
    workflow_coordination::{Assignment, WorkerId},
    workflow_jobs::{Delivery, JobLease, JobOperation, JobOutcome, JobSpec},
};
use zeroship_workflow_manager::Options;

#[compio::test]
async fn postgres_delivery_receipt_failure_rolls_back_checkpoint() {
    let fixture = PostgresFixture::start().await;
    let admin = connect(&fixture.admin_url).await;
    let (service, app, _, _deployments) = registered_service(Rc::new(fixture.store.clone())).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    let job = publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    admin.batch_execute("CREATE FUNCTION customer.fail_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected receipt failure'; END $$; CREATE TRIGGER fail_receipt BEFORE UPDATE ON customer.__zeroship_workflow_job_receipts FOR EACH ROW EXECUTE FUNCTION customer.fail_receipt();").await.unwrap();
    let checkpoint = execution(
        json!([{"kind":"Sleep", "ordinal":0, "name":"delay", "nameOccurrence":0, "wakeAt":chrono::Utc::now()+chrono::Duration::hours(1)}]),
    );
    assert!(scope
        .complete_job(&claimed, &grant, checkpoint.clone())
        .await
        .is_err());
    admin.batch_execute("DROP TRIGGER fail_receipt ON customer.__zeroship_workflow_job_receipts; DROP FUNCTION customer.fail_receipt();").await.unwrap();
    assert!(scope.job_receipt(&job).await.unwrap().is_none());
    assert!(scope.pending_jobs(None, 10).await.unwrap().is_empty());
    let tx = service.begin().await.unwrap();
    for table in ["steps", "waits"] {
        assert_eq!(
            journal_count(&tx, table, json!({"app_id":app.as_str()})).await,
            0
        );
    }
    let tasks = journal_rows(&tx, "tasks", json!({"id":claimed.assignment().id})).await;
    assert_eq!(tasks[0].text("state").unwrap(), "leased");
    tx.commit().await.unwrap();
    assert_eq!(
        scope
            .complete_job(&claimed, &grant, checkpoint)
            .await
            .unwrap()
            .outcome,
        JobOutcome::Waiting
    );
    assert_eq!(scope.pending_jobs(None, 10).await.unwrap().len(), 1);
}

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let dir = tempfile::tempdir().unwrap();
            $contract(Rc::new(
                sqlite_store(&dir.path().join("zs-workflow.sqlite")).await,
            ))
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            $contract(Rc::new(fixture.store.clone())).await;
        }
    };
}

case!(
    sqlite_delivery_receipts_survive_history,
    postgres_delivery_receipts_survive_history,
    receipts
);
case!(
    sqlite_delivery_fences_attempts_and_legacy_tasks,
    postgres_delivery_fences_attempts_and_legacy_tasks,
    attempts
);
case!(
    sqlite_delivery_checkpoint_publishes_successor,
    postgres_delivery_checkpoint_publishes_successor,
    checkpoint
);
case!(
    sqlite_delivery_creator_policy_bounds_execution,
    postgres_delivery_creator_policy_bounds_execution,
    policy_bounds
);

case!(
    sqlite_delivery_rejects_competing_frontiers,
    postgres_delivery_rejects_competing_frontiers,
    competing
);

async fn competing(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    let job = publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    let competing = JobSpec {
        id: zeroship_core::workflow_jobs::JobId::mint(),
        ..job.clone()
    };
    manager.queue.submit(&competing).await.unwrap();
    let duplicate = manager.queue.claim(&owner).await.unwrap().unwrap();
    assert_eq!(duplicate.delivery().job, competing);
    assert!(matches!(
        scope.accept_job(&duplicate).await.unwrap(),
        JobAcceptance::Deferred
    ));
    scope
        .complete_job(
            &claimed,
            &grant,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    let JobAcceptance::Settled(stale) = scope.accept_job(&duplicate).await.unwrap() else {
        panic!("expected obsolete frontier rejection")
    };
    assert_eq!(stale.outcome, JobOutcome::Rejected);
    manager
        .queue
        .settle(&owner, &stale.settlement(&duplicate).unwrap())
        .await
        .unwrap();
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(&tx, "tasks", json!({"app_id":app.as_str()})).await,
        1
    );
    tx.commit().await.unwrap();
}

case!(
    sqlite_delivery_cancels_lock_waits,
    postgres_delivery_cancels_lock_waits,
    lock_waits
);

async fn lock_waits(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();

    for expiry in [true, false] {
        let mut held = service.begin().await.unwrap();
        super::super::app::lock_app(&mut held, &app).await.unwrap();
        let revision = if expiry { 2 } else { 3 };
        let policy = AppPolicy {
            lease_ms: if expiry { 200 } else { 60_000 },
            ..Default::default()
        };
        service
            .policies
            .fixture_install(&app, leased_policy(revision, policy))
            .unwrap();
        let mut accepting = Box::pin(scope.accept_job(&grant));
        assert!(futures::poll!(accepting.as_mut()).is_pending());
        if expiry {
            // The operation must cancel while the creator lock is still held.
            let result = compio::time::timeout(Duration::from_secs(1), accepting)
                .await
                .expect("delivery must cancel while the app lock is held");
            assert!(
                matches!(result, Err(WorkflowServiceError::Timeout)),
                "{result:?}"
            );
            held.commit().await.unwrap();
        } else {
            service
                .policies
                .fixture_install(
                    &app,
                    leased_policy(
                        4,
                        AppPolicy {
                            dispatch: false,
                            ..Default::default()
                        },
                    ),
                )
                .unwrap();
            held.commit().await.unwrap();
            let result = accepting.await;
            assert!(
                matches!(result, Err(WorkflowServiceError::Unavailable(_))),
                "{result:?}"
            );
        }
        let tx = service.begin().await.unwrap();
        for table in ["tasks", "job_receipts"] {
            assert_eq!(
                journal_count(&tx, table, json!({"app_id":app.as_str()})).await,
                0
            );
        }
        tx.commit().await.unwrap();
    }
    assert!(grant.remaining().is_some());
    service
        .policies
        .fixture_install(&app, leased_policy(5, AppPolicy::default()))
        .unwrap();
    task(scope.accept_job(&grant).await.unwrap());
}

fn assignment(app: &AppId) -> Assignment {
    Assignment {
        app_id: app.clone(),
        worker_id: WorkerId::mint(),
        revision: 1.try_into().unwrap(),
        expires_at: (chrono::Utc::now().timestamp_millis() + 120_000)
            .try_into()
            .unwrap(),
    }
}

async fn publish(scope: &AppWorkflows, manager: &Manager) -> JobSpec {
    let job = scope.pending_jobs(None, 1).await.unwrap().remove(0);
    assert_eq!(
        scope
            .publish_job(&job.id, &Publisher::new(&scope.app, manager.queue.clone()))
            .await
            .unwrap(),
        job
    );
    job
}

fn task(acceptance: JobAcceptance) -> DeliveredTask {
    match acceptance {
        JobAcceptance::Execute(task) => *task,
        other => panic!("expected execution, got {other:?}"),
    }
}

// A deliberately malformed trusted-host implementation probes immutable journal
// identity. Production callers use the native manager or authenticated client.
struct ProbeLease {
    delivery: Delivery,
    expires: Instant,
}
impl ProbeLease {
    fn copy(grant: &impl JobLease) -> Self {
        let now = Instant::now();
        Self {
            delivery: grant.delivery().clone(),
            expires: now + grant.remaining().unwrap(),
        }
    }
}
impl JobLease for ProbeLease {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
    }
}

async fn receipts(store: Rc<OrmStore>) {
    let (service, app, other, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input: json!({"private":"creator-input"}),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let manager = Manager::with_options(
        &app,
        Options {
            lease: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .await;
    let owner = assignment(&app);
    let job = publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    assert!(matches!(
        service.fixture_app(other).accept_job(&grant).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    assert_eq!(claimed.assignment().invocation.run_id, run.id);
    assert!(matches!(
        scope.accept_job(&grant).await.unwrap(),
        JobAcceptance::Deferred
    ));
    let mut changed = ProbeLease::copy(&grant);
    changed.delivery.job.available_at = (job.available_at.get() + 1).try_into().unwrap();
    assert!(matches!(
        scope.accept_job(&changed).await,
        Err(WorkflowServiceError::Conflict(_))
    ));

    let execution =
        execution(json!([{"kind":"RunCompleted", "output":{"private":"creator-output"}}]));
    let receipt = scope
        .complete_job(&claimed, &grant, execution.clone())
        .await
        .unwrap();
    assert_eq!(receipt.job, job);
    assert_eq!(receipt.outcome, JobOutcome::Completed);
    assert!(!serde_json::to_string(&receipt)
        .unwrap()
        .contains("creator-output"));
    let mut expired = ProbeLease::copy(&grant);
    expired.expires = Instant::now();
    assert_eq!(
        scope
            .complete_job(&claimed, &expired, execution)
            .await
            .unwrap(),
        receipt
    );
    assert!(matches!(
        scope
            .complete_job(
                &claimed,
                &expired,
                super::execution(json!([{"kind":"RunCompleted", "output":"changed"}]))
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    // Model a committed creator result whose manager ACK was never delivered.
    expire(&grant).await;
    let redelivered = manager.queue.claim(&owner).await.unwrap().unwrap();
    assert!(redelivered.delivery().attempt > grant.delivery().attempt);
    let JobAcceptance::Settled(recovered) = scope.accept_job(&redelivered).await.unwrap() else {
        panic!("redelivery must read the committed result")
    };
    assert_eq!(recovered, receipt);
    let command = receipt.settlement(&redelivered).unwrap();
    let settled = manager.queue.settle(&owner, &command).await.unwrap();
    assert_eq!(
        manager.queue.settle(&owner, &command).await.unwrap(),
        settled
    );
    assert!(manager.queue.claim(&owner).await.unwrap().is_none());

    retained_history(&scope, &job, &receipt, &expired, &changed.delivery.job).await;
}

async fn retained_history(
    scope: &AppWorkflows,
    job: &JobSpec,
    receipt: &crate::service::delivery::JobReceipt,
    expired: &ProbeLease,
    changed: &JobSpec,
) {
    let service = &scope.service;
    let tx = service.begin().await.unwrap();
    for table in ["tasks", "generations", "runs"] {
        tx.database()
            .collection(&format!("__zeroship_workflow_{table}"))
            .unwrap()
            .execute(zeroship_data_orm::orm::Operation::Purge {
                filter: zeroship_data_orm::value!({"app_id":scope.app.as_str()}),
                many: true,
            })
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let reopened = WorkflowService::open(service.store.clone(), service.policies.clone())
        .await
        .unwrap()
        .fixture_app(scope.app.clone());
    assert_eq!(
        reopened.job_receipt(job).await.unwrap(),
        Some(receipt.clone())
    );
    let JobAcceptance::Settled(replayed) = reopened.accept_job(expired).await.unwrap() else {
        panic!("expected retained receipt")
    };
    assert_eq!(&replayed, receipt);
    assert!(reopened.job_receipt(changed).await.is_err());
}

async fn attempts(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::with_options(
        &app,
        Options {
            lease: Duration::from_millis(700),
            ..Default::default()
        },
    )
    .await;
    let owner = assignment(&app);
    publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    let worker = WorkerIdentity::new(owner.worker_id.as_str().into()).unwrap();
    let a = claimed.assignment();
    assert_eq!(
        service.heartbeat(&worker, &a.id, &a.token).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert_eq!(
        service
            .complete(
                &worker,
                &a.id,
                &a.token,
                execution(json!([{"kind":"RunCompleted"}]))
            )
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert_eq!(
        service.release(&worker, &a.id, &a.token).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    scope.release_job(&claimed, &grant).await.unwrap();
    scope.release_job(&claimed, &grant).await.unwrap();
    assert!(service.poll(&worker).await.unwrap().is_none());
    expire(&grant).await;
    let next = manager.queue.claim(&owner).await.unwrap().unwrap();
    assert!(next.delivery().attempt > grant.delivery().attempt);
    let replacement = task(scope.accept_job(&next).await.unwrap());
    assert_ne!(
        replacement.assignment().token.as_str(),
        claimed.assignment().token.as_str()
    );
    assert!(scope.release_job(&claimed, &next).await.is_err());
    assert!(scope
        .complete_job(&claimed, &next, execution(json!([{"kind":"RunCompleted"}])))
        .await
        .is_err());
    assert!(scope
        .complete_job(
            &claimed,
            &grant,
            execution(json!([{"kind":"RunCompleted"}]))
        )
        .await
        .is_err());
    let receipt = scope
        .complete_job(
            &replacement,
            &next,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    manager
        .queue
        .settle(&owner, &receipt.settlement(&next).unwrap())
        .await
        .unwrap();
}

async fn expire(grant: &impl JobLease) {
    if let Some(remaining) = grant.remaining() {
        compio::time::sleep(remaining + Duration::from_millis(10)).await;
    }
}

async fn checkpoint(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    let original = publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    let receipt = scope
        .complete_job(
            &claimed,
            &grant,
            execution(json!([{
                "kind":"Sleep", "ordinal":0, "name":"delay", "nameOccurrence":0,
                "wakeAt":chrono::Utc::now() + chrono::Duration::hours(1)
            }])),
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Waiting);
    let pending = scope.pending_jobs(None, 10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        matches!(&pending[0].operation, JobOperation::Advance {run_id, revision, ..} if run_id.as_str()==run.id && revision.get()==2)
    );
    assert_ne!(pending[0].id, original.id);
    assert!(pending[0].available_at > original.available_at);
    let worker = WorkerIdentity::new(owner.worker_id.as_str().into()).unwrap();
    let tx = service.begin().await.unwrap();
    journal_update(&tx, "runs", json!({"id":run.id}), json!({"due_at":0})).await;
    tx.commit().await.unwrap();
    assert!(service.poll(&worker).await.unwrap().is_none());
    assert!(matches!(
        scope.accept_job(&grant).await.unwrap(),
        JobAcceptance::Settled(_)
    ));
    manager
        .queue
        .settle(&owner, &receipt.settlement(&grant).unwrap())
        .await
        .unwrap();
    assert!(manager.queue.claim(&owner).await.unwrap().is_none());
    assert_eq!(publish(&scope, &manager).await, pending[0]);
}

async fn policy_bounds(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    dispatch: false,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    assert!(matches!(
        scope.accept_job(&grant).await.unwrap(),
        JobAcceptance::Deferred
    ));
    assert!(scope
        .job_receipt(&grant.delivery().job)
        .await
        .unwrap()
        .is_none());
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                3,
                AppPolicy {
                    lease_ms: 700,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    assert!(claimed.remaining().unwrap() < grant.remaining().unwrap());
    assert!(claimed.assignment().lease_ms <= 700);
    let renewed_grant = manager
        .queue
        .heartbeat(&owner, grant.delivery())
        .await
        .unwrap();
    let (renewed, control) = scope.heartbeat_job(&claimed, &renewed_grant).await.unwrap();
    assert_eq!(control, crate::service::ControlIntent::None);
    assert_eq!(
        renewed.assignment().token.as_str(),
        claimed.assignment().token.as_str()
    );
    compio::time::sleep(renewed.remaining().unwrap() + Duration::from_millis(10)).await;
    assert!(renewed_grant.remaining().is_some());
    assert_eq!(
        scope
            .heartbeat_job(&renewed, &renewed_grant)
            .await
            .unwrap_err(),
        WorkflowServiceError::Timeout
    );
    assert_eq!(
        scope
            .complete_job(
                &renewed,
                &renewed_grant,
                execution(json!([{"kind":"RunCompleted"}]))
            )
            .await,
        Err(WorkflowServiceError::Timeout)
    );
    assert_eq!(
        scope.release_job(&renewed, &renewed_grant).await,
        Err(WorkflowServiceError::Timeout)
    );
    let fresh = task(scope.accept_job(&renewed_grant).await.unwrap());
    assert_ne!(
        fresh.assignment().token.as_str(),
        claimed.assignment().token.as_str()
    );
}
