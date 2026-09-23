use super::*;
use crate::{
    backend::WorkflowBackend,
    service::{app::lock_app, publication::JobPublisher},
};
use std::cell::Cell;
use futures::FutureExt;
use zeroship_core::{
    workflow_coordination::WorkerId,
    workflow_jobs::{Delivery, JobLease, JobSpec},
};

#[compio::test]
async fn sqlite_backend_queue_keeps_original_deadline_after_refresh() {
    let directory = tempfile::tempdir().unwrap();
    queued_backend(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    ))
    .await;
}

#[compio::test]
async fn postgres_backend_queue_keeps_original_deadline_after_refresh() {
    let fixture = PostgresFixture::start().await;
    queued_backend(Rc::new(fixture.store.clone())).await;
}

#[expect(
    clippy::future_not_send,
    reason = "the fixture drives a native app journal"
)]
async fn queued_backend(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let binding = service.policies.current_binding(&app).unwrap();
    let objects = crate::service::tests::objects::Objects::new();
    let backend = service
        .bind_app(&binding)
        .unwrap()
        .into_backend(
            &service,
            crate::service::tests::objects::StepOutputs::shared(&objects, 1024),
            objects.stager(),
        )
        .unwrap();
    let before = ingress_state(&service, &app).await;
    let mut blocker = service.begin().await.unwrap();
    lock_app(&mut blocker, &app).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), deadline)
                .unwrap()
                .with_ingress_epoch(Some(open_epoch())),
        )
        .unwrap();
    let mut pending = backend
        .start("Example".into(), serde_json::Value::Null, StartOptions::default())
        .boxed_local();
    // Polling enqueues without yielding to the local dispatch task. Refresh then
    // precedes dequeue, while the app lock keeps the eventual mutation blocked.
    assert!(futures::poll!(pending.as_mut()).is_pending());
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(
                2.try_into().unwrap(),
                AppPolicy::default(),
                deadline + Duration::from_secs(30),
            )
            .unwrap()
            .with_ingress_epoch(Some(open_epoch())),
        )
        .unwrap();
    assert!(matches!(
        compio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap(),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    binding.authority().unwrap().check().unwrap();
    blocker.commit().await.unwrap();
    assert_eq!(ingress_state(&service, &app).await, before);
    backend
        .start("Example".into(), serde_json::Value::Null, StartOptions::default())
        .await
        .unwrap();
}

struct Publisher {
    app: AppId,
    calls: Cell<usize>,
}

struct Lease {
    delivery: Delivery,
    expires: Instant,
}
impl JobLease for Lease {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires.checked_duration_since(Instant::now())
    }
}
impl JobPublisher for Publisher {
    fn app_id(&self) -> &AppId {
        &self.app
    }
    fn submit(
        &self,
        job: &JobSpec,
    ) -> impl std::future::Future<Output = Result<JobSpec, WorkflowServiceError>> {
        self.calls.set(self.calls.get() + 1);
        std::future::ready(Ok(job.clone()))
    }
}

#[compio::test]
async fn sqlite_retired_publication_handle_only_replays_committed_receipt() {
    let directory = tempfile::tempdir().unwrap();
    Box::pin(retired_publication(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    )))
    .await;
}

#[compio::test]
async fn postgres_retired_publication_handle_only_replays_committed_receipt() {
    let fixture = PostgresFixture::start().await;
    Box::pin(retired_publication(Rc::new(fixture.store.clone()))).await;
}

#[expect(
    clippy::future_not_send,
    reason = "publication fixtures use the native app journal"
)]
async fn retired_publication(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let old = service.fixture_app(app.clone());
    let publisher = Publisher {
        app: app.clone(),
        calls: Cell::new(0),
    };
    old.start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let confirmed = old.pending_jobs(None, 1).await.unwrap().remove(0);
    assert_eq!(
        old.publish_job(&confirmed.id, &publisher).await.unwrap(),
        confirmed
    );
    old.start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let pending = old.pending_jobs(None, 1).await.unwrap().remove(0);
    let replacement = service.policies.bind(app.clone()).unwrap();
    replacement
        .begin_refresh()
        .unwrap()
        .install(leased_policy(1, AppPolicy::default()))
        .unwrap();
    let before = ingress_state(&service, &app).await;
    let calls = publisher.calls.get();
    assert!(matches!(
        old.pending_jobs(None, 1).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(matches!(
        old.publish_job(&pending.id, &publisher).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(
        old.publish_job(&confirmed.id, &publisher).await.unwrap(),
        confirmed
    );
    assert_eq!(publisher.calls.get(), calls);
    assert_eq!(ingress_state(&service, &app).await, before);
    let current = service.bind_app(&replacement).unwrap();
    assert_eq!(
        current.pending_jobs(None, 1).await.unwrap().as_slice(),
        std::slice::from_ref(&pending)
    );
    assert_eq!(
        current.publish_job(&pending.id, &publisher).await.unwrap(),
        pending
    );
    assert_eq!(publisher.calls.get(), calls + 1);

    let captured_scope = current
        .with_authority(replacement.authority().unwrap())
        .unwrap();
    let lease = Lease {
        delivery: Delivery {
            job: pending.clone(),
            worker_id: WorkerId::mint(),
            assignment_revision: 1.try_into().unwrap(),
            attempt: 1.try_into().unwrap(),
            deadline: 1.try_into().unwrap(),
        },
        expires: Instant::now() + Duration::from_secs(30),
    };
    let crate::service::delivery::JobAcceptance::Execute(task) =
        captured_scope.accept_job(&lease).await.unwrap()
    else {
        panic!("published run must be executable")
    };
    let receipt = captured_scope
        .complete_job(&task, &lease, execution(json!([{"kind":"RunCompleted"}])))
        .await
        .unwrap();
    replacement.revoke().unwrap();
    assert_eq!(
        captured_scope.job_receipt(&pending).await.unwrap(),
        Some(receipt)
    );
    assert_eq!(
        captured_scope
            .status(&task.assignment().invocation.run_id)
            .await
            .unwrap()
            .state,
        RunState::Completed
    );
    assert!(matches!(
        captured_scope.pending_jobs(None, 1).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
}

