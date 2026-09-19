//! The delivered release job gives a superseded deployment's journal hold back
//! only when nothing in the customer journal still depends on it.
#![expect(
    clippy::future_not_send,
    reason = "release tests own compio-local journals and clients"
)]

use super::*;
use crate::{
    deployment_holds::{DeploymentHoldClient, HoldGeneration, HoldReceipt, HoldScope},
    operations::RunOperation,
    service::delivery::ATTEMPT_IO_CEILING,
};
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

/// A platform client that never answers, so the only thing that can end an
/// attempt waiting on it is that attempt's own budget.
struct Stalled(deployment_fixture::OwnedClient);
#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for Stalled {
    fn scope(&self) -> &HoldScope {
        self.0.scope()
    }
    async fn acquire(
        &self,
        _deployment: &str,
        _generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        futures::future::pending().await
    }
    async fn release(
        &self,
        _deployment: &str,
        _generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        futures::future::pending().await
    }
}

#[compio::test]
async fn sqlite_release_job_ends_a_stalled_attempt_at_the_io_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    io_ceiling(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_release_job_ends_a_stalled_attempt_at_the_io_ceiling() {
    let fixture = PostgresFixture::start().await;
    io_ceiling(Rc::new(fixture.store.clone())).await;
}

/// One release attempt ends at the journal I/O ceiling, not at the end of the
/// authority it captured. The stall sits in the platform hold client, so the
/// window measured here is the attempt's own budget and not a journal wait.
///
/// This pins the composition, not the magnitude. Both arms move with
/// [`ATTEMPT_IO_CEILING`], so retuning the ceiling keeps them green; what fails
/// is dropping the ceiling term and handing one attempt its whole authority.
async fn io_ceiling(store: Rc<OrmStore>) {
    let (service, app, other, platform) = registered_service(store).await;
    let client = platform.client(&app);
    let service = service.with_deployments(
        platform
            .binding(&[&app, &other])
            .with_hold_client(Rc::new(Stalled(client.clone()))),
    );
    let superseded = platform.deploy(&app).await;
    service
        .acquire_deployment_hold(&app, &superseded.id, &superseded.hash, &client)
        .await
        .unwrap();
    service.activate_deploy(&app, &superseded).await.unwrap();
    let current = platform.deploy(&app).await;
    service
        .acquire_deployment_hold(&app, &current.id, &current.hash, &client)
        .await
        .unwrap();
    service.activate_deploy(&app, &current).await.unwrap();
    let scope = service.fixture_app(app.clone());
    drain(&scope).await;

    let mut lease = Lease::release(&app, &superseded.id);
    lease.expires = Instant::now() + ATTEMPT_IO_CEILING * 6;
    assert!(
        lease.remaining().unwrap() > ATTEMPT_IO_CEILING * 3,
        "the fixture authority is narrower than the window asserted below, so it \
         would bound this attempt instead of the ceiling"
    );
    let started = Instant::now();
    let result = scope.release_hold_job(&lease).await;
    let capped = started.elapsed();
    assert!(
        matches!(result, Err(WorkflowServiceError::Timeout)),
        "{result:?}"
    );
    assert!(
        capped >= ATTEMPT_IO_CEILING,
        "the attempt ended before the ceiling, so something other than its budget \
         stopped it and this measures nothing: {capped:?}"
    );
    assert!(
        capped < ATTEMPT_IO_CEILING * 3,
        "one attempt was handed authority beyond the ceiling: {capped:?}"
    );

    // The control moves one variable. An authority narrower than the ceiling
    // binds the same stalled attempt instead, so the arm above is not a fixed
    // wait that would pass with the ceiling term removed.
    let narrow = ATTEMPT_IO_CEILING / 5;
    let mut short = Lease::release(&app, &superseded.id);
    short.expires = Instant::now() + narrow;
    let started = Instant::now();
    let result = scope.release_hold_job(&short).await;
    let bounded = started.elapsed();
    assert!(
        matches!(result, Err(WorkflowServiceError::Timeout)),
        "{result:?}"
    );
    assert!(
        bounded >= narrow,
        "the control ended before its own authority, so it bounded nothing: {bounded:?}"
    );
    assert!(
        bounded < ATTEMPT_IO_CEILING,
        "the control was capped by the ceiling too, so the arm above proves \
         nothing: {bounded:?}"
    );
    assert!(scope
        .job_receipt(&lease.delivery.job)
        .await
        .unwrap()
        .is_none());
    platform.assert_held(&app, &superseded.id).await;
}
