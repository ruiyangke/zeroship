//! The service's maintenance lane, end to end against its own journal and the
//! queue it owns.
//!
//! Nothing here places the app on a worker. That is the point: the lane asserts
//! its own authority, so a claim that succeeds without any `assignments` row is
//! what the authority seam decided.
#![expect(
    clippy::future_not_send,
    reason = "journal and queue fixtures stay on their compio runtime"
)]

#[path = "support/holds.rs"]
mod holds;
#[path = "support/journal.rs"]
mod journal;
#[allow(
    dead_code,
    reason = "the shared platform fixture also supports process tests"
)]
#[path = "support/platform.rs"]
mod platform;

use futures::future::LocalBoxFuture;
use std::{
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{RunId, WorkerId},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobOutcome, JobSpec},
    workflow_policy::AppPolicy,
};
use zeroship_workflow::{service::maintenance::MaintenanceOptions, WorkflowServiceError};
use zeroship_workflow_manager::{
    policy::{PolicyObservation, PolicySource},
    recovery::Options as RecoveryOptions,
    Error as ManagerError, Queue,
};
use zeroship_workflow_server::{
    coordinator::{connect_eligibility, Coordinator, Options},
    runs::RunService,
    sweeps::{MaintenanceLane, SweepError, Swept},
};

/// One observation, so binding installs a real lease rather than a stub.
#[derive(Debug)]
struct Source(PolicyObservation);
impl PolicySource for Source {
    fn observe<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<PolicyObservation, ManagerError>> {
        Box::pin(async move {
            if self.0.app_id() == app {
                Ok(self.0.clone())
            } else {
                Err(ManagerError::Denied)
            }
        })
    }
    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, ManagerError> {
        Ok(observation.expires_at())
    }
}

struct Fixture {
    platform: platform::Platform,
    queue: Queue,
    lane: MaintenanceLane,
    app: AppId,
}

impl Fixture {
    async fn new() -> Self {
        let platform = platform::Platform::new().await;
        let eligibility = Rc::new(
            connect_eligibility(&platform.runtime_url, Options::default())
                .await
                .unwrap(),
        );
        let service = Coordinator::connect(
            &platform.runtime_url,
            Options::default(),
            holds::client(),
            eligibility,
        )
        .await
        .unwrap();
        let app = AppId::mint();
        platform.seed_app(&app).await;
        let queue = service.manager.queue().clone();
        queue.register_scope(&app).await.unwrap();
        // The journal rows an app needs to exist at all. The lane's operation
        // acts on what this does NOT seed: there is no pending publication, so
        // the reconciliation page is empty and its phase advances.
        journal::seed_run(&platform, &app).await;
        let runs = Rc::new(
            RunService::connect(
                &platform.runtime_url,
                service.recovery(RecoveryOptions::default()).unwrap(),
            )
            .await
            .unwrap(),
        );
        let policies = Rc::new(Source(
            PolicyObservation::new(
                app.clone(),
                7.try_into().unwrap(),
                AppPolicy::default(),
                Instant::now() + Duration::from_mins(10),
            )
            .unwrap(),
        ));
        let lane = MaintenanceLane::new(
            queue.clone(),
            runs,
            policies as Rc<dyn PolicySource>,
            WorkerId::mint(),
            MaintenanceOptions::default(),
        )
        .unwrap();
        Self {
            platform,
            queue,
            lane,
            app,
        }
    }

    async fn row(&self, job: &JobId) -> (String, Option<String>, Option<String>) {
        let row = self
            .platform
            .admin
            .query_one(
                "SELECT state, outcome, worker_id FROM workflow_manager.jobs WHERE id=$1",
                &[&job.as_str()],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1), row.get(2))
    }
}

fn sweep(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Reconcile {},
        available_at: 0.try_into().unwrap(),
    }
}

fn creator_work(app: &AppId) -> JobSpec {
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

/// The lane claims a maintenance row without a placement, runs it against the
/// service's own journal and records what it committed.
///
/// The creator row submitted first is the control for the claim: it sits ahead
/// in the dispatch order and is still `ready` afterwards, so what the lane took
/// was chosen rather than simply first.
#[compio::test]
async fn the_lane_claims_and_settles_a_journal_only_maintenance_job() {
    let fixture = Box::pin(Fixture::new()).await;
    let creator = creator_work(&fixture.app);
    let maintenance = sweep(&fixture.app);
    fixture.queue.submit(&creator).await.unwrap();
    fixture.queue.submit(&maintenance).await.unwrap();
    assert_eq!(fixture.row(&creator.id).await.0, "ready");
    assert_eq!(fixture.row(&maintenance.id).await.0, "ready");

    let swept = Box::pin(fixture.lane.sweep(&fixture.app)).await.unwrap();
    let Swept::Settled(receipt) = swept else {
        panic!("the lane settles a reconciliation page: {swept:?}");
    };
    assert_eq!(receipt.job_id, maintenance.id);
    // The publications phase always asks for another page, so the first sweep
    // of an app settles as waiting rather than completed.
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});

    let (state, outcome, worker) = fixture.row(&maintenance.id).await;
    assert_eq!(state, "settled");
    assert_eq!(
        outcome,
        Some(serde_json::to_string(&JobOutcome::Waiting {}).unwrap())
    );
    assert_eq!(
        worker.as_deref(),
        Some(fixture.lane.identity().as_str()),
        "the row must carry the identity the lane asserted for itself"
    );
    assert_eq!(
        fixture.row(&creator.id).await.0,
        "ready",
        "creator work stays for the claimant that runs it"
    );

    // Nothing else the lane can take: the creator row at the head is not it.
    assert!(matches!(
        Box::pin(fixture.lane.sweep(&fixture.app)).await.unwrap(),
        Swept::Idle
    ));
}

/// A maintenance row whose operation needs an artifact source is claimed and
/// then refused by name.
///
/// The lane takes every kind the dispatch runs, and the ones reaching
/// `self.service.deployments` have no source here. The refusal must reach the
/// caller rather than be absorbed: the row stays unsettled and redeliverable,
/// and what is missing is named.
#[compio::test]
async fn an_operation_without_an_artifact_source_is_refused_by_name() {
    let fixture = Box::pin(Fixture::new()).await;
    let release = JobSpec {
        id: JobId::mint(),
        app_id: fixture.app.clone(),
        operation: JobOperation::ReleaseHold {
            deployment_id: DeploymentId::mint(),
        },
        available_at: 0.try_into().unwrap(),
    };
    fixture.queue.submit(&release).await.unwrap();
    assert_eq!(fixture.row(&release.id).await.0, "ready");

    let refused = Box::pin(fixture.lane.sweep(&fixture.app)).await;
    let Err(SweepError::Journal(WorkflowServiceError::Unavailable(reason))) = refused else {
        panic!("the release names its missing source: {refused:?}");
    };
    assert!(
        reason.contains("deployment"),
        "the refusal must name what is missing, not just fail: {reason}"
    );
    let (state, outcome, worker) = fixture.row(&release.id).await;
    assert_eq!(state, "leased", "a refused row keeps its lease until it lapses");
    assert_eq!(outcome, None);
    assert_eq!(worker.as_deref(), Some(fixture.lane.identity().as_str()));
}
