//! The delivered release job gives a superseded deployment's journal hold back
//! only when nothing in the customer journal still depends on it.
#![expect(
    clippy::future_not_send,
    reason = "release tests own compio-local journals and clients"
)]

use super::*;
use crate::operations::RunOperation;
use std::time::{Duration, Instant};
use zeroship_core::workflow_jobs::{
    Delivery, DeploymentId, JobId, JobLease, JobOperation, JobOutcome, JobSpec,
};

struct Lease {
    delivery: Delivery,
    expires: Instant,
}
impl Lease {
    fn release(app: &AppId, deployment: &str) -> Self {
        Self::operation(
            app,
            JobOperation::ReleaseHold {
                deployment_id: DeploymentId::parse(deployment).unwrap(),
            },
        )
    }
    fn operation(app: &AppId, operation: JobOperation) -> Self {
        Self {
            delivery: Delivery {
                job: JobSpec {
                    id: JobId::mint(),
                    app_id: app.clone(),
                    operation,
                    available_at: 0.try_into().unwrap(),
                },
                worker_id: zeroship_core::workflow_coordination::WorkerId::mint(),
                assignment_revision: 1.try_into().unwrap(),
                attempt: 1.try_into().unwrap(),
                deadline: 0.try_into().unwrap(),
            },
            expires: Instant::now() + Duration::from_secs(3600),
        }
    }
}
impl JobLease for Lease {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
    }
}

#[compio::test]
async fn sqlite_release_job_refuses_a_needed_deployment_and_gives_back_an_unused_one() {
    let dir = tempfile::tempdir().unwrap();
    release_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_release_job_refuses_a_needed_deployment_and_gives_back_an_unused_one() {
    let fixture = PostgresFixture::start().await;
    release_contract(Rc::new(fixture.store.clone())).await;
}

async fn release_contract(store: Rc<OrmStore>) {
    let (service, app, other, platform) = registered_service(store).await;
    let client = platform.client(&app);
    let first = platform.deploy(&app).await;
    service
        .acquire_deployment_hold(&app, &first.id, &first.hash, &client)
        .await
        .unwrap();
    service.activate_deploy(&app, &first).await.unwrap();
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();

    // Republishing supersedes the first deployment without discarding its runs.
    let second = platform.deploy(&app).await;
    service
        .acquire_deployment_hold(&app, &second.id, &second.hash, &client)
        .await
        .unwrap();
    service.activate_deploy(&app, &second).await.unwrap();
    // With the outbox drained, the run itself is the only reason to refuse.
    drain(&scope).await;

    // A live run pinned to the deployment refuses the release, retryably.
    let pinned = Lease::release(&app, &first.id);
    let refused = scope.release_hold_job(&pinned).await.unwrap();
    assert_eq!(refused.outcome, JobOutcome::Waiting {});
    assert_eq!(refused.job, pinned.delivery.job);
    assert_eq!(pinned.delivery.job.deployment_id(), None);
    platform.assert_held(&app, &first.id).await;
    // The committed refusal replays without asking the platform again.
    assert_eq!(scope.release_hold_job(&pinned).await.unwrap(), refused);

    // Finishing the run leaves its generation, which a partial restart needs.
    scope
        .transition(&RequestId::mint(), &run.id, RunOperation::Cancel)
        .await
        .unwrap();
    drain(&scope).await;
    assert_eq!(
        scope
            .release_hold_job(&Lease::release(&app, &first.id))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Waiting {}
    );
    platform.assert_held(&app, &first.id).await;
    // A failed release rolled its admission fence back.
    service.activate_deploy(&app, &first).await.unwrap();

    // A superseded deployment the journal never used is given back, and the
    // platform collector can then reclaim it.
    let third = platform.deploy(&app).await;
    service
        .acquire_deployment_hold(&app, &third.id, &third.hash, &client)
        .await
        .unwrap();
    service.activate_deploy(&app, &third).await.unwrap();
    drain(&scope).await;
    platform.assert_held(&app, &second.id).await;
    let released = Lease::release(&app, &second.id);
    assert_eq!(
        scope.release_hold_job(&released).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
    platform.assert_reclaimable(&app, &second.id).await;
    // Replay stays Completed without a second platform release.
    assert_eq!(
        scope.release_hold_job(&released).await.unwrap().outcome,
        JobOutcome::Completed {}
    );

    // A deployment this journal never held is already given back.
    let unknown = platform.deploy(&app).await;
    assert_eq!(
        scope
            .release_hold_job(&Lease::release(&app, &unknown.id))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );

    // Foreign scopes and other operations are refused before any journal work.
    assert!(matches!(
        scope
            .release_hold_job(&Lease::release(&other, &third.id))
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(matches!(
        scope
            .release_hold_job(&Lease::operation(&app, JobOperation::Collect {}))
            .await,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
}
