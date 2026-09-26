//! The three sweeps that record a `deploys` row, on a host holding no creator
//! artifacts.
//!
//! `activate_job`, `cron_job` and `management_job`'s latest target each need a
//! deployment's workflow declarations. A host holding the bundle derives them
//! from the bytes; the workflow service holds no blob store, so it takes the
//! same summary as an assertion and records its own row from that.
//!
//! Every contract here runs BOTH arms. The control is the same host with the
//! retention authority alone: it must refuse each sweep by name, or a green on
//! the asserted arm would only be saying the sweep asks for nothing. A third arm
//! answers about the right deployment under the wrong hash, which the caller's
//! own hold has to reject.

#![expect(
    clippy::future_not_send,
    reason = "asserted registration contracts own compio journal I/O"
)]

use super::*;
use crate::service::{
    deploys, AppDeployments, IntervalAnchor, ScheduleCatchUp, ScheduleOverlap,
    ScheduleRegistration, ScheduleTiming,
};
use deployment_fixture::RehashedRegistrations;
use std::time::{Duration, Instant};
use zeroship_core::{
    workflow_coordination::{ManagementOutcome, RunId, RunState, WorkerId},
    workflow_jobs::{
        Delivery, DeploymentId, JobId, JobLease, JobOperation, JobOutcome, JobSpec,
        ManagementCommand,
    },
    workflow_schedules::ScheduleId,
};

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
    sqlite_activation_records_an_asserted_registration_without_artifacts,
    postgres_activation_records_an_asserted_registration_without_artifacts,
    activation
);
case!(
    sqlite_cron_admits_an_occurrence_against_an_asserted_registration,
    postgres_cron_admits_an_occurrence_against_an_asserted_registration,
    cron
);
case!(
    sqlite_management_latest_pins_an_asserted_registration,
    postgres_management_latest_pins_an_asserted_registration,
    management
);

/// A deployment declaring one workflow and one schedule that names it.
fn declaration() -> DeployRegistration {
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
            input: json!({"creator": "input"}),
            overlap: ScheduleOverlap::Allow,
            catch_up: ScheduleCatchUp::default(),
        }],
    }
}

/// The same journal and policies, rebound to a different deployment host.
async fn rebound(service: &WorkflowService, deployments: AppDeployments) -> WorkflowService {
    WorkflowService::open(service.store.clone(), service.policies.clone())
        .await
        .unwrap()
        .with_deployments(deployments)
}

/// The refusal a host holding neither capability owes. It must NAME the missing
/// capability rather than read as an absent or damaged deployment.
fn assert_uncapable(error: &WorkflowServiceError) {
    assert!(
        matches!(error, WorkflowServiceError::Unavailable(message)
            if message.contains("artifact store") && message.contains("registration source")),
        "the refusal must name both missing capabilities: {error}"
    );
}

/// The registration the journal committed for `deployment`.
async fn recorded(service: &WorkflowService, app: &AppId, deployment: &str) -> DeployRegistration {
    let tx = service.begin().await.unwrap();
    let row = deploys::read(&tx, app, deployment)
        .await
        .unwrap()
        .expect("the sweep recorded a deploys row");
    let registration = row.registration().unwrap();
    tx.commit().await.unwrap();
    registration
}

fn activate_grant(app: &AppId, deployment: &DeployRegistration, revision: i64) -> Grant {
    Grant::new(
        app,
        JobOperation::Activate {
            deployment_id: DeploymentId::parse(&deployment.id).unwrap(),
            revision: revision.try_into().unwrap(),
        },
    )
}

async fn activation(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let published = platform
        .publish(&app, &declaration(), &Sources::default())
        .await
        .unwrap();

    // The control: retention authority and nothing else.
    let holds = rebound(&service, platform.holds_binding(&[&app])).await;
    let refused = holds
        .fixture_app(app.clone())
        .activate_job(&activate_grant(&app, &published, 1))
        .await
        .unwrap_err();
    assert_uncapable(&refused);

    // The hash arm: the right deployment, a hash this host does not hold.
    let rehashed = rebound(
        &service,
        platform
            .holds_binding(&[&app])
            .with_registrations(Rc::new(RehashedRegistrations(
                platform.registrations(),
                "b".repeat(64),
            ))),
    )
    .await;
    let conflict = rehashed
        .fixture_app(app.clone())
        .activate_job(&activate_grant(&app, &published, 1))
        .await
        .unwrap_err();
    assert!(
        matches!(&conflict, WorkflowServiceError::Conflict(message)
            if message.contains("deployment hash")),
        "an assertion under another hash is refused: {conflict}"
    );

    // And the asserted arm, which commits.
    let asserted = rebound(&service, platform.asserted_binding(&[&app])).await;
    assert!(
        asserted
            .deployments
            .as_ref()
            .unwrap()
            .read(&app, &published.hash)
            .await
            .is_err(),
        "the asserted source must not have opened an artifact path"
    );
    let grant = activate_grant(&app, &published, 1);
    let receipt = asserted
        .fixture_app(app.clone())
        .activate_job(&grant)
        .await
        .unwrap();
    assert_eq!(receipt.job, grant.delivery.job);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    // What the journal recorded is the published manifest's own summary,
    // schedules and inline creator input included.
    assert_eq!(recorded(&asserted, &app, &published.id).await, published);
    platform.assert_held(&app, &published.id).await;
}

async fn cron(store: Rc<OrmStore>) {
    let objects = objects::Objects::new();
    let (service, app, _, platform) = registered_service(store).await;
    let published = platform
        .publish(&app, &declaration(), &Sources::default())
        .await
        .unwrap();
    let asserted = rebound(&service, platform.asserted_binding(&[&app])).await;
    let scope = asserted.fixture_app(app.clone());
    assert_eq!(
        scope
            .activate_job(&activate_grant(&app, &published, 1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );

    let schedule = ScheduleId::mint();
    let cron = |app: &AppId| {
        Grant::new(
            app,
            JobOperation::Cron {
                deployment_id: DeploymentId::parse(&published.id).unwrap(),
                schedule_id: schedule.clone(),
                schedule_name: "periodic".into(),
                request_id: RequestId::mint(),
                run_id: RunId::mint(),
                revision: 1.try_into().unwrap(),
                scheduled_at: 1000.try_into().unwrap(),
            },
        )
    };

    // The control, on the journal this activation already wrote: the occurrence
    // is refused for the missing capability rather than admitted from the row.
    let holds = rebound(&service, platform.holds_binding(&[&app])).await;
    let refused = holds
        .fixture_app(app.clone())
        .cron_job(&cron(&app), &objects)
        .await
        .unwrap_err();
    assert_uncapable(&refused);

    let grant = cron(&app);
    let JobOperation::Cron { run_id, .. } = &grant.delivery.job.operation else {
        panic!("cron fixture");
    };
    let run_id = run_id.as_str().to_owned();
    assert_eq!(
        scope.cron_job(&grant, &objects).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
    let tx = asserted.begin().await.unwrap();
    let runs = journal_rows(&tx, "runs", json!({"app_id":app.as_str(), "id":run_id})).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text("deploy_id").unwrap(), published.id);
    tx.commit().await.unwrap();
    assert_eq!(recorded(&asserted, &app, &published.id).await, published);
}

async fn management(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let published = platform
        .publish(&app, &declaration(), &Sources::default())
        .await
        .unwrap();
    let asserted = rebound(&service, platform.asserted_binding(&[&app])).await;
    let scope = asserted.fixture_app(app.clone());
    assert_eq!(
        scope
            .activate_job(&activate_grant(&app, &published, 1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap()
        .id;
    let latest = |app: &AppId, revision: i64| {
        Grant::new(
            app,
            JobOperation::Management {
                request_id: RequestId::mint(),
                run_id: RunId::parse(&run).unwrap(),
                revision: revision.try_into().unwrap(),
                command: ManagementCommand::RestartLatest {
                    deployment_id: DeploymentId::parse(&published.id).unwrap(),
                },
            },
        )
    };

    let holds = rebound(&service, platform.holds_binding(&[&app])).await;
    let refused = holds
        .fixture_app(app.clone())
        .management_job(&latest(&app, 1))
        .await
        .unwrap_err();
    assert_uncapable(&refused);

    let receipt = scope.management_job(&latest(&app, 1)).await.unwrap();
    assert_eq!(
        receipt.outcome,
        JobOutcome::Management {
            outcome: ManagementOutcome::Restarted {
                state: RunState::Queued,
                restarted_from_ordinal: None,
                pinned_to: DeploymentId::parse(&published.id).unwrap(),
            }
        }
    );
    assert_eq!(recorded(&asserted, &app, &published.id).await, published);
}
