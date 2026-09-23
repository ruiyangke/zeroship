#![expect(
    clippy::future_not_send,
    reason = "creator cron contracts use compio-local storage"
)]

use super::*;
use crate::service::{
    AppWorkflows, IntervalAnchor, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
};
use std::time::{Duration, Instant};
use zeroship_core::{
    workflow_coordination::{RunId, WorkerId},
    workflow_jobs::{Delivery, DeploymentId, JobId, JobLease, JobOperation, JobOutcome, JobSpec},
    workflow_schedules::ScheduleId,
};
use zeroship_data_orm::{orm::Entity, value};

#[derive(Clone)]
struct Grant {
    delivery: Delivery,
    expires: Instant,
}
impl Grant {
    fn new(app: &AppId, operation: JobOperation) -> Self {
        Self {
            delivery: Delivery {
                job: JobSpec {
                    id: JobId::mint(),
                    app_id: app.clone(),
                    operation,
                    available_at: 0.try_into().unwrap(),
                },
                worker_id: WorkerId::mint(),
                assignment_revision: 1.try_into().unwrap(),
                attempt: 1.try_into().unwrap(),
                deadline: 0.try_into().unwrap(),
            },
            expires: Instant::now() + Duration::from_secs(30),
        }
    }
    fn cron(
        app: &AppId,
        deploy: &DeployRegistration,
        schedule: &ScheduleId,
        revision: i64,
        at: i64,
    ) -> Self {
        Self::new(
            app,
            JobOperation::Cron {
                deployment_id: DeploymentId::parse(&deploy.id).unwrap(),
                schedule_id: schedule.clone(),
                schedule_name: "periodic".into(),
                request_id: RequestId::mint(),
                run_id: RunId::mint(),
                revision: revision.try_into().unwrap(),
                scheduled_at: at.try_into().unwrap(),
            },
        )
    }
    fn run_id(&self) -> &str {
        let JobOperation::Cron { run_id, .. } = &self.delivery.job.operation else {
            panic!("cron fixture")
        };
        run_id.as_str()
    }
    fn retry(&self) -> Self {
        let mut result = self.clone();
        result.delivery.attempt = (result.delivery.attempt.get() + 1).try_into().unwrap();
        result.expires = Instant::now() + Duration::from_secs(30);
        result
    }
}
impl JobLease for Grant {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }
}

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            Box::pin($contract(Rc::new(
                sqlite_store(&directory.path().join("creator.sqlite")).await,
            )))
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

case!(
    sqlite_cron_commits_exact_run_and_replays_without_artifacts,
    postgres_cron_commits_exact_run_and_replays_without_artifacts,
    replay
);
case!(
    sqlite_cron_keeps_historical_activation_input_after_replacement,
    postgres_cron_keeps_historical_activation_input_after_replacement,
    historical
);
case!(
    sqlite_cron_rejects_rebound_occurrence_and_schedule_identities,
    postgres_cron_rejects_rebound_occurrence_and_schedule_identities,
    identities
);
case!(
    sqlite_cron_overlap_skip_survives_continuation_and_redelivery,
    postgres_cron_overlap_skip_survives_continuation_and_redelivery,
    overlap
);
case!(
    sqlite_cron_capacity_and_policy_refusals_remain_retryable,
    postgres_cron_capacity_and_policy_refusals_remain_retryable,
    capacity
);
case!(
    sqlite_cron_reacquires_released_journal_retention,
    postgres_cron_reacquires_released_journal_retention,
    reacquire
);
case!(
    sqlite_cron_requires_readiness_and_exact_available_artifacts,
    postgres_cron_requires_readiness_and_exact_available_artifacts,
    prerequisites
);
case!(
    sqlite_cron_authority_expires_while_waiting_for_app_lock,
    postgres_cron_authority_expires_while_waiting_for_app_lock,
    authority
);
case!(
    sqlite_cron_rechecks_policy_after_app_lock,
    postgres_cron_rechecks_policy_after_app_lock,
    policy_lock
);
case!(
    sqlite_cron_receipt_replay_requires_exact_occurrence_linkage,
    postgres_cron_receipt_replay_requires_exact_occurrence_linkage,
    linkage
);

fn scheduled(input: serde_json::Value, overlap: ScheduleOverlap) -> DeployRegistration {
    DeployRegistration {
        id: typed_id::generate("dep"),
        hash: String::new(),
        workflows: ["Example".into()].into(),
        schedules: vec![ScheduleRegistration {
            name: "periodic".into(),
            workflow_name: "Example".into(),
            schedule: ScheduleTiming::Interval {
                interval_ms: 60_000,
                anchor: IntervalAnchor::Epoch,
            },
            input,
            overlap,
            catch_up: Default::default(),
        }],
    }
}

async fn publish(
    platform: &Deployments,
    app: &AppId,
    input: serde_json::Value,
    overlap: ScheduleOverlap,
) -> DeployRegistration {
    platform
        .publish(app, &scheduled(input, overlap), &Sources::default())
        .await
        .unwrap()
}

async fn activate(scope: &AppWorkflows, deploy: &DeployRegistration, revision: i64) {
    let grant = Grant::new(
        scope.app_id(),
        JobOperation::Activate {
            deployment_id: DeploymentId::parse(&deploy.id).unwrap(),
            revision: revision.try_into().unwrap(),
        },
    );
    assert_eq!(
        scope.activate_job(&grant).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
}

async fn count(service: &WorkflowService, app: &AppId, table: &str) -> i64 {
    let tx = service.begin().await.unwrap();
    let count = journal_count(&tx, table, json!({"app_id":app.as_str()})).await;
    tx.commit().await.unwrap();
    count
}

async fn assert_unaccepted(service: &WorkflowService, scope: &AppWorkflows, grant: &Grant) {
    assert!(scope
        .job_receipt(&grant.delivery.job)
        .await
        .unwrap()
        .is_none());
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(
            &tx,
            "runs",
            json!({"app_id":scope.app_id().as_str(), "id":grant.run_id()})
        )
        .await,
        0
    );
    assert_eq!(
        journal_count(
            &tx,
            "occurrences",
            json!({"app_id":scope.app_id().as_str(), "job_id":grant.delivery.job.id.as_str()})
        )
        .await,
        0
    );
    assert!(advance_intents(&tx, scope.app_id(), grant.run_id(), None)
        .await
        .is_empty());
    tx.commit().await.unwrap();
}

async fn replay(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store.clone()).await;
    let deployment = publish(
        &platform,
        &app,
        json!({"creator":"input"}),
        ScheduleOverlap::Allow,
    )
    .await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    let grant = Grant::cron(&app, &deployment, &ScheduleId::mint(), 1, 1000);
    let receipt = scope.cron_job(&grant, &objects).await.unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    let tx = service.begin().await.unwrap();
    let runs = journal_rows(&tx, "runs", json!({"app_id":app.as_str()})).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text("id").unwrap(), grant.run_id());
    assert_eq!(runs[0].text("deploy_id").unwrap(), deployment.id);
    let generations = journal_rows(
        &tx,
        "generations",
        json!({"app_id":app.as_str(), "run_id":grant.run_id()}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&generations[0].text("input").unwrap()).unwrap(),
        json!({"creator":"input"})
    );
    assert_eq!(
        journal_count(&tx, "tasks", json!({"app_id":app.as_str()})).await,
        0
    );
    tx.commit().await.unwrap();
    let jobs = scope.pending_jobs(None, 10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].deployment_id().unwrap().as_str(), deployment.id);
    assert!(
        matches!(&jobs[0].operation, JobOperation::Advance { run_id, generation: 0, .. } if run_id.as_str() == grant.run_id())
    );
    platform
        .source
        .delete_manifest(&app, &deployment.hash)
        .await
        .unwrap();
    let reopened = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    reopened
        .fixture_register(&app, leased_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    let mut expired = grant.retry();
    expired.expires = Instant::now();
    assert_eq!(
        reopened
            .fixture_app(app.clone())
            .cron_job(&expired, &objects)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(scope.cron_job(&grant.retry(), &objects).await.unwrap(), receipt);
    assert_eq!(scope.pending_jobs(None, 10).await.unwrap(), jobs);
    assert_eq!(count(&service, &app, "runs").await, 1);
    assert_eq!(count(&service, &app, "occurrences").await, 1);
}

async fn historical(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let old = publish(&platform, &app, json!("original"), ScheduleOverlap::Allow).await;
    let newer = publish(
        &platform,
        &app,
        json!("replacement"),
        ScheduleOverlap::Allow,
    )
    .await;
    let removed = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &newer, 2).await;
    activate(&scope, &old, 1).await;
    let schedule = ScheduleId::mint();
    let current = Grant::cron(&app, &newer, &schedule, 2, 1000);
    let prior = Grant::cron(&app, &old, &schedule, 1, 1000);
    scope.cron_job(&current, &objects).await.unwrap();
    activate(&scope, &removed, 3).await;
    scope.cron_job(&prior, &objects).await.unwrap();
    let tx = service.begin().await.unwrap();
    let selected = journal_rows(&tx, "deploys", json!({"app_id":app.as_str(), "active":1})).await;
    assert_eq!(selected[0].text("id").unwrap(), removed.id);
    for (grant, input) in [(&prior, "original"), (&current, "replacement")] {
        let rows = journal_rows(
            &tx,
            "generations",
            json!({"app_id":app.as_str(), "run_id":grant.run_id()}),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&rows[0].text("input").unwrap()).unwrap(),
            json!(input)
        );
    }
    assert_eq!(
        journal_count(&tx, "occurrences", json!({"app_id":app.as_str()})).await,
        2
    );
    assert_eq!(
        journal_count(&tx, "schedules", json!({"app_id":app.as_str()})).await,
        1
    );
    tx.commit().await.unwrap();
}

async fn identities(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, other, platform) = registered_service(store).await;
    let mut registration = scheduled(json!(null), ScheduleOverlap::Allow);
    let mut second = registration.schedules[0].clone();
    second.name = "other".into();
    registration.schedules.push(second);
    let deployment = platform
        .publish(&app, &registration, &Sources::default())
        .await
        .unwrap();
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    let schedule = ScheduleId::mint();
    let grant = Grant::cron(&app, &deployment, &schedule, 1, 1000);
    assert!(matches!(
        service.fixture_app(other).cron_job(&grant, &objects).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    scope.cron_job(&grant, &objects).await.unwrap();
    for field in [
        "run", "request", "schedule", "name", "revision", "instant", "job", "deploy",
    ] {
        let mut bad = grant.retry();
        let JobOperation::Cron {
            deployment_id,
            schedule_id,
            schedule_name,
            request_id,
            run_id,
            revision,
            scheduled_at,
        } = &mut bad.delivery.job.operation
        else {
            unreachable!()
        };
        match field {
            "run" => *run_id = RunId::mint(),
            "request" => *request_id = RequestId::mint(),
            "schedule" => *schedule_id = ScheduleId::mint(),
            "name" => *schedule_name = "other".into(),
            "revision" => *revision = 2.try_into().unwrap(),
            "instant" => *scheduled_at = 2000.try_into().unwrap(),
            "job" => bad.delivery.job.id = JobId::mint(),
            "deploy" => *deployment_id = DeploymentId::mint(),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                scope.cron_job(&bad, &objects).await,
                Err(WorkflowServiceError::Conflict(_))
            ),
            "changed {field} accepted"
        );
    }
    let mut rebound = Grant::cron(&app, &deployment, &schedule, 1, 2000);
    let JobOperation::Cron { schedule_name, .. } = &mut rebound.delivery.job.operation else {
        unreachable!()
    };
    *schedule_name = "other".into();
    assert!(matches!(
        scope.cron_job(&rebound, &objects).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let renamed = Grant::cron(&app, &deployment, &ScheduleId::mint(), 1, 2000);
    assert!(matches!(
        scope.cron_job(&renamed, &objects).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert_eq!(count(&service, &app, "runs").await, 1);
    assert_eq!(count(&service, &app, "occurrences").await, 1);
}

async fn finish_run(service: &WorkflowService, app: &AppId, continuation: bool) {
    let scope = service.fixture_app(app.clone());
    let mut pending = None;
    for job in scope.pending_jobs(None, 10).await.unwrap() {
        if scope.job_receipt(&job).await.unwrap().is_none() {
            pending = Some(job);
            break;
        }
    }
    let grant = Grant {
        delivery: Delivery {
            job: pending.expect("cron must publish an unconsumed Advance job"),
            worker_id: WorkerId::mint(),
            assignment_revision: 1.try_into().unwrap(),
            attempt: 1.try_into().unwrap(),
            deadline: 0.try_into().unwrap(),
        },
        expires: Instant::now() + Duration::from_secs(30),
    };
    let crate::service::delivery::JobAcceptance::Execute(task) =
        scope.accept_job(&grant).await.unwrap()
    else {
        panic!("published Advance job must claim its exact run");
    };
    let outcome = if continuation {
        json!([{"kind":"ContinueAsNew", "input":"continued"}])
    } else {
        json!([{"kind":"RunCompleted"}])
    };
    scope
        .complete_job(&task, &grant, execution(outcome))
        .await
        .unwrap();
}

async fn overlap(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = publish(&platform, &app, json!(null), ScheduleOverlap::SkipIfRunning).await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    let schedule = ScheduleId::mint();
    let first = Grant::cron(&app, &deployment, &schedule, 1, 1000);
    scope.cron_job(&first, &objects).await.unwrap();
    finish_run(&service, &app, true).await;
    let skipped = Grant::cron(&app, &deployment, &schedule, 1, 2000);
    let skipped_receipt = scope.cron_job(&skipped, &objects).await.unwrap();
    assert_eq!(skipped_receipt.outcome, JobOutcome::Rejected {});
    assert_eq!(count(&service, &app, "runs").await, 2);
    finish_run(&service, &app, false).await;
    assert_eq!(
        scope.cron_job(&skipped.retry(), &objects).await.unwrap(),
        skipped_receipt
    );
    let next = Grant::cron(&app, &deployment, &schedule, 1, 3000);
    assert_eq!(
        scope.cron_job(&next, &objects).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(
        &tx,
        "occurrences",
        json!({"app_id":app.as_str(), "job_id":skipped.delivery.job.id.as_str()}),
    )
    .await;
    assert!(rows[0].optional_text("run_id").unwrap().is_none());
    tx.commit().await.unwrap();
}

async fn capacity(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = publish(
        &platform,
        &app,
        json!({"static":true}),
        ScheduleOverlap::Allow,
    )
    .await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    service
        .fixture_register(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    max_live_runs: 1,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    let schedule = ScheduleId::mint();
    scope
        .cron_job(&Grant::cron(&app, &deployment, &schedule, 1, 1000), &objects)
        .await
        .unwrap();
    let retry = Grant::cron(&app, &deployment, &schedule, 1, 2000);
    assert!(matches!(
        scope.cron_job(&retry, &objects).await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    assert_unaccepted(&service, &scope, &retry).await;
    finish_run(&service, &app, false).await;
    service
        .fixture_register(
            &app,
            leased_policy(
                3,
                AppPolicy {
                    admission: false,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        scope.cron_job(&retry, &objects).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_unaccepted(&service, &scope, &retry).await;
    service
        .fixture_register(
            &app,
            leased_policy(
                4,
                AppPolicy {
                    max_input_bytes: 1,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        scope.cron_job(&retry, &objects).await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));
    assert_unaccepted(&service, &scope, &retry).await;
    service
        .fixture_register(&app, leased_policy(5, AppPolicy::default()))
        .await
        .unwrap();
    assert_eq!(
        scope.cron_job(&retry.retry(), &objects).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
}

async fn reacquire(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = publish(&platform, &app, json!("retained"), ScheduleOverlap::Allow).await;
    let replacement = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    activate(&scope, &replacement, 2).await;
    let client = platform.client(&app);
    service
        .release_deployment_hold(&app, &deployment.id, &client)
        .await
        .unwrap();
    let tx = service.begin().await.unwrap();
    let before = journal_rows(
        &tx,
        "deployment_holds",
        json!({"app_id":app.as_str(), "deploy_id":deployment.id}),
    )
    .await;
    assert_eq!(before[0].text("state").unwrap(), "released");
    tx.commit().await.unwrap();
    let grant = Grant::cron(&app, &deployment, &ScheduleId::mint(), 1, 1000);
    scope.cron_job(&grant, &objects).await.unwrap();
    platform.assert_held(&app, &deployment.id).await;
    let tx = service.begin().await.unwrap();
    let after = journal_rows(
        &tx,
        "deployment_holds",
        json!({"app_id":app.as_str(), "deploy_id":deployment.id}),
    )
    .await;
    assert_eq!(after[0].text("state").unwrap(), "held");
    assert!(after[0].integer("generation").unwrap() > before[0].integer("generation").unwrap());
    tx.commit().await.unwrap();
    assert!(service
        .release_deployment_hold(&app, &deployment.id, &client)
        .await
        .is_err());
}

async fn prerequisites(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = publish(&platform, &app, json!(null), ScheduleOverlap::Allow).await;
    let scope = service.fixture_app(app.clone());
    let grant = Grant::cron(&app, &deployment, &ScheduleId::mint(), 1, 1000);
    assert!(matches!(
        scope.cron_job(&grant, &objects).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_unaccepted(&service, &scope, &grant).await;
    activate(&scope, &deployment, 1).await;
    let bytes = platform
        .source
        .get_manifest(&app, &deployment.hash)
        .await
        .unwrap();
    platform
        .source
        .delete_manifest(&app, &deployment.hash)
        .await
        .unwrap();
    assert!(matches!(
        scope.cron_job(&grant, &objects).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_unaccepted(&service, &scope, &grant).await;
    platform
        .source
        .put_manifest(&app, &deployment.hash, b"{}")
        .await
        .unwrap();
    assert!(matches!(
        scope.cron_job(&grant, &objects).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_unaccepted(&service, &scope, &grant).await;
    platform
        .source
        .delete_manifest(&app, &deployment.hash)
        .await
        .unwrap();
    platform
        .source
        .put_manifest(&app, &deployment.hash, &bytes)
        .await
        .unwrap();
    scope.cron_job(&grant.retry(), &objects).await.unwrap();
}

async fn authority(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = publish(&platform, &app, json!(null), ScheduleOverlap::Allow).await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    let grant = Grant::cron(&app, &deployment, &ScheduleId::mint(), 1, 1000);
    let mut expired = grant.clone();
    expired.expires = Instant::now();
    assert!(matches!(
        scope.cron_job(&expired, &objects).await,
        Err(WorkflowServiceError::Timeout)
    ));
    let mut held = service.begin().await.unwrap();
    super::super::app::lock_app(&mut held, &app).await.unwrap();
    let mut short = grant.clone();
    short.expires = Instant::now() + Duration::from_millis(100);
    assert!(matches!(
        scope.cron_job(&short, &objects).await,
        Err(WorkflowServiceError::Timeout)
    ));
    held.commit().await.unwrap();
    assert_unaccepted(&service, &scope, &grant).await;
    scope.cron_job(&grant.retry(), &objects).await.unwrap();
}

async fn policy_lock(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = publish(&platform, &app, json!(null), ScheduleOverlap::Allow).await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    let grant = Grant::cron(&app, &deployment, &ScheduleId::mint(), 1, 1000);
    let mut held = service.begin().await.unwrap();
    super::super::app::lock_app(&mut held, &app).await.unwrap();
    let mut work = Box::pin(scope.cron_job(&grant, &objects));
    assert!(futures::poll!(work.as_mut()).is_pending());
    service
        .policies
        .fixture_install(&app, leased_policy(2, AppPolicy::default()))
        .unwrap();
    held.commit().await.unwrap();
    assert!(matches!(
        work.await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_unaccepted(&service, &scope, &grant).await;
    scope.cron_job(&grant.retry(), &objects).await.unwrap();
}

async fn linkage(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = publish(&platform, &app, json!(null), ScheduleOverlap::Allow).await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    let schedule = ScheduleId::mint();
    let first = Grant::cron(&app, &deployment, &schedule, 1, 1000);
    let second = Grant::cron(&app, &deployment, &schedule, 1, 2000);
    let receipt = scope.cron_job(&first, &objects).await.unwrap();
    scope.cron_job(&second, &objects).await.unwrap();
    let tx = service.begin().await.unwrap();
    let original = journal_rows(
        &tx,
        "occurrences",
        json!({"app_id":app.as_str(), "job_id":first.delivery.job.id.as_str()}),
    )
    .await
    .remove(0);
    let activation = journal_rows(
        &tx,
        "activations",
        json!({"app_id":app.as_str(), "revision":1}),
    )
    .await
    .remove(0);
    tx.commit().await.unwrap();
    for (field, changed) in [
        ("run_id", json!(second.run_id())),
        ("job_id", json!(activation.text("id").unwrap())),
        ("revision", json!(2)),
        ("at", json!(99_000)),
    ] {
        let tx = service.begin().await.unwrap();
        journal_update(
            &tx,
            "occurrences",
            json!({"app_id":app.as_str(), "id":original.text("id").unwrap()}),
            json!({field:changed}),
        )
        .await;
        tx.commit().await.unwrap();
        assert!(
            matches!(
                scope.cron_job(&first.retry(), &objects).await,
                Err(WorkflowServiceError::Internal(_))
            ),
            "changed occurrence {field} replayed"
        );
        let tx = service.begin().await.unwrap();
        journal_update(
            &tx,
            "occurrences",
            json!({"app_id":app.as_str(), "id":original.text("id").unwrap()}),
            json!({field:serde_json::to_value(&original.0[field]).unwrap()}),
        )
        .await;
        tx.commit().await.unwrap();
    }
    assert_eq!(scope.cron_job(&first.retry(), &objects).await.unwrap(), receipt);
    let tx = service.begin().await.unwrap();
    tx.database()
        .collection(super::super::models::occurrences::Entity::COLLECTION)
        .unwrap()
        .delete(value!({"app_id":app.as_str(), "job_id":first.delivery.job.id.as_str()}))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        scope.cron_job(&first.retry(), &objects).await,
        Err(WorkflowServiceError::Internal(_))
    ));
    assert_eq!(count(&service, &app, "runs").await, 2);
}

enum ReceiptFault {
    Sqlite(std::path::PathBuf),
    Postgres(Box<compio_postgres::Client>),
}
impl ReceiptFault {
    async fn set(&self, enabled: bool) {
        match self {
            Self::Sqlite(path) => {
                let path = path.clone();
                compio::runtime::spawn_blocking(move || {
                    let connection = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
                    connection.execute_batch(if enabled {
                        "CREATE TRIGGER cron_receipt_fault BEFORE UPDATE OF outcome ON __zeroship_workflow_job_receipts BEGIN SELECT RAISE(ABORT, 'cron receipt fault'); END"
                    } else { "DROP TRIGGER cron_receipt_fault" }).unwrap();
                }).await.unwrap();
            }
            Self::Postgres(client) => client.batch_execute(if enabled {
                "CREATE FUNCTION customer.cron_receipt_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'cron receipt fault'; END $$; CREATE TRIGGER cron_receipt_fault BEFORE UPDATE OF outcome ON customer.__zeroship_workflow_job_receipts FOR EACH ROW EXECUTE FUNCTION customer.cron_receipt_fault();"
            } else { "DROP TRIGGER cron_receipt_fault ON customer.__zeroship_workflow_job_receipts; DROP FUNCTION customer.cron_receipt_fault();" }).await.unwrap(),
        }
    }
}

#[compio::test]
async fn sqlite_cron_receipt_failure_rolls_back_run_publication_and_occurrence() {
    let directory = tempfile::tempdir().unwrap();
    let store = Rc::new(sqlite_store(&directory.path().join("creator.sqlite")).await);
    let attached = store
        .backend
        .query(
            &store.binding,
            "PRAGMA database_list",
            &[],
        )
        .await
        .unwrap();
    let namespace = store
        .backend
        .namespace(&store.binding);
    let path = attached
        .iter()
        .find(|row| row.get("name").and_then(zeroship_data_orm::Value::as_str) == Some(namespace))
        .and_then(|row| row.get("file"))
        .and_then(zeroship_data_orm::Value::as_str)
        .map(std::path::PathBuf::from)
        .unwrap();
    Box::pin(rollback(store, ReceiptFault::Sqlite(path))).await;
}

#[compio::test]
async fn postgres_cron_receipt_failure_rolls_back_run_publication_and_occurrence() {
    let fixture = PostgresFixture::start().await;
    let admin = connect(&fixture.admin_url).await;
    Box::pin(rollback(
        Rc::new(fixture.store.clone()),
        ReceiptFault::Postgres(Box::new(admin)),
    ))
    .await;
}

async fn rollback(store: Rc<OrmStore>, fault: ReceiptFault) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = publish(
        &platform,
        &app,
        json!({"atomic":true}),
        ScheduleOverlap::Allow,
    )
    .await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    let grant = Grant::cron(&app, &deployment, &ScheduleId::mint(), 1, 1000);
    fault.set(true).await;
    assert!(scope.cron_job(&grant, &objects).await.is_err());
    assert_unaccepted(&service, &scope, &grant).await;
    for table in [
        "schedules",
        "runs",
        "generations",
        "outbox",
        "job_publications",
        "occurrences",
    ] {
        assert_eq!(
            count(&service, &app, table).await,
            0,
            "{table} escaped the failed transaction"
        );
    }
    fault.set(false).await;
    assert_eq!(
        scope.cron_job(&grant.retry(), &objects).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
    for table in [
        "schedules",
        "runs",
        "generations",
        "outbox",
        "job_publications",
        "occurrences",
    ] {
        assert_eq!(
            count(&service, &app, table).await,
            1,
            "{table} missing from accepted transaction"
        );
    }
}

#[derive(Debug)]
struct Gate {
    entered: flume::Sender<()>,
    resume: flume::Receiver<()>,
}
impl Gate {
    async fn wait(self) {
        self.entered.send_async(()).await.unwrap();
        let _ = self.resume.recv_async().await;
    }
}
fn gate() -> (Gate, flume::Receiver<()>, flume::Sender<()>) {
    let (entered, observe) = flume::bounded(1);
    let (resume, wait) = flume::bounded(1);
    (
        Gate {
            entered,
            resume: wait,
        },
        observe,
        resume,
    )
}

struct HeldClient {
    inner: deployment_fixture::OwnedClient,
    gate: std::cell::RefCell<Option<Gate>>,
}
#[async_trait::async_trait(?Send)]
impl crate::deployment_holds::DeploymentHoldClient for HeldClient {
    fn scope(&self) -> &crate::deployment_holds::HoldScope {
        self.inner.scope()
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: crate::deployment_holds::HoldGeneration,
    ) -> Result<crate::deployment_holds::HoldReceipt, WorkflowServiceError> {
        let receipt = self.inner.acquire(deployment, generation).await?;
        let gate = self.gate.borrow_mut().take();
        if let Some(gate) = gate {
            gate.wait().await;
        }
        Ok(receipt)
    }
    async fn release(
        &self,
        deployment: &str,
        generation: crate::deployment_holds::HoldGeneration,
    ) -> Result<crate::deployment_holds::HoldReceipt, WorkflowServiceError> {
        self.inner.release(deployment, generation).await
    }
}

case!(
    sqlite_cron_policy_change_fences_held_retention_io,
    postgres_cron_policy_change_fences_held_retention_io,
    policy_hold
);
case!(
    sqlite_cron_expiry_fences_held_retention_io,
    postgres_cron_expiry_fences_held_retention_io,
    expired_hold
);

async fn policy_hold(store: Rc<OrmStore>) {
    Box::pin(held_authority(store, false)).await;
}
async fn expired_hold(store: Rc<OrmStore>) {
    Box::pin(held_authority(store, true)).await;
}

async fn held_authority(store: Rc<OrmStore>, expire: bool) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let deployment = publish(&platform, &app, json!("original"), ScheduleOverlap::Allow).await;
    let replacement = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    activate(&scope, &deployment, 1).await;
    activate(&scope, &replacement, 2).await;
    service
        .release_deployment_hold(&app, &deployment.id, &platform.client(&app))
        .await
        .unwrap();
    let (gate, entered, resume) = gate();
    let client = Rc::new(HeldClient {
        inner: platform.client(&app),
        gate: std::cell::RefCell::new(Some(gate)),
    });
    let gated = service
        .clone()
        .with_deployments(platform.binding(&[&app]).with_hold_client(client));
    let scope = gated.fixture_app(app.clone());
    let mut grant = Grant::cron(&app, &deployment, &ScheduleId::mint(), 1, 1000);
    if expire {
        grant.expires = Instant::now() + Duration::from_secs(2);
    }
    let (result, ()) = futures::join!(scope.cron_job(&grant, &objects), async {
        compio::time::timeout(Duration::from_secs(5), entered.recv_async())
            .await
            .expect("retention acquisition must reach its gated response")
            .unwrap();
        if expire {
            compio::time::sleep(
                grant.expires.saturating_duration_since(Instant::now()) + Duration::from_millis(20),
            )
            .await;
        } else {
            service
                .policies
                .fixture_install(&app, leased_policy(2, AppPolicy::default()))
                .unwrap();
        }
        let _ = resume.send_async(()).await;
    });
    if expire {
        assert!(matches!(result, Err(WorkflowServiceError::Timeout)));
    } else {
        assert!(matches!(result, Err(WorkflowServiceError::Unavailable(_))));
    }
    assert_unaccepted(&service, &scope, &grant).await;
    platform.assert_held(&app, &deployment.id).await;
    assert_eq!(
        service
            .fixture_app(app)
            .cron_job(&grant.retry(), &objects)
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
}
