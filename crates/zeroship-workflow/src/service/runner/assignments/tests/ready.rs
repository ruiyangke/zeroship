//! Request-path publication of prepared app backends and their intents.

use super::*;
use crate::{
    backend::WorkflowBackend,
    operations::StartOptions,
    service::runner::publication::{self, HostTransport},
};
use zeroship_core::workflow_jobs::{
    Delivery, JobId, JobOperation, JobOutcome, JobSpec, Settlement,
};

async fn start(backend: &dyn WorkflowBackend) -> Result<String, WorkflowServiceError> {
    backend
        .start("Example".into(), StartOptions::default())
        .await
        .map(|run| run.id)
}

#[compio::test]
async fn backend_is_published_only_after_preparation_and_withdrawn_on_removal() {
    let fixture = Fixture::new();
    let scope = scope();
    let (observed, release) = fixture.factory.gate(&scope.app_id);
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.refresh(&scope));
    exchanges.push(fixture.page(None, &[]));
    peer(&fixture, exchanges, async |client| {
        let (consumer, _probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        let requests = fixture.ready.backend(scope.app_id.clone());
        let mut preparing = Box::pin(bindings.reconcile());
        assert!(matches!(
            futures::future::select(observed, preparing.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        // The generation being prepared holds live authority, yet ingress is
        // not admitted until preparation passes its final checks.
        fixture.factory.calls()[0]
            .policy
            .authority()
            .unwrap()
            .check()
            .unwrap();
        assert!(!fixture.ready.is_ready(&scope.app_id));
        not_ready(requests.status(run_id()).await);
        release.send(()).unwrap();
        preparing.await.unwrap();
        assert!(fixture.ready.is_ready(&scope.app_id));
        // The same request handle now reaches this app's engine.
        assert!(matches!(
            requests.status(run_id()).await,
            Err(WorkflowServiceError::NotFound(_))
        ));
        let retained = fixture.factory.calls()[0]
            .runtime
            .borrow()
            .as_ref()
            .unwrap()
            .backend
            .clone();
        bindings.reconcile().await.unwrap();
        assert!(!fixture.ready.is_ready(&scope.app_id));
        not_ready(requests.status(run_id()).await);
        // A client cloned before removal keeps its retired generation.
        unavailable(start(&retained).await);
    })
    .await;
}

#[compio::test]
async fn a_request_backend_from_another_generation_is_refused_and_never_published() {
    let fixture = Fixture::new();
    fixture.factory.foreign_backend();
    let scope = scope();
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.refresh(&scope));
    peer(&fixture, exchanges, async |client| {
        let (mut consumer, probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        assert!(matches!(
            bindings.reconcile().await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
        assert!(!fixture.ready.is_ready(&scope.app_id));
        not_ready(
            fixture
                .ready
                .backend(scope.app_id.clone())
                .status(run_id())
                .await,
        );
        assert!(claims(&mut consumer, &probe).await.is_empty());
    })
    .await;
}

#[compio::test]
async fn replacement_and_closure_withdraw_published_backends() {
    let fixture = Fixture::new();
    let original = scope();
    let replacement = AssignedScope {
        assignment_revision: 2.try_into().unwrap(),
        ..original.clone()
    };
    let mut exchanges = fixture.scan(std::slice::from_ref(&original));
    exchanges.extend(fixture.refresh(&original));
    exchanges.extend(fixture.scan(std::slice::from_ref(&replacement)));
    exchanges.extend(fixture.refresh(&replacement));
    peer(&fixture, exchanges, async |client| {
        let (consumer, _probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        let requests = fixture.ready.backend(original.app_id.clone());
        bindings.reconcile().await.unwrap();
        assert!(fixture.ready.is_ready(&original.app_id));
        let (observed, release) = fixture.factory.gate(&original.app_id);
        let mut replacing = Box::pin(bindings.reconcile());
        assert!(matches!(
            futures::future::select(observed, replacing.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        // The old generation is withdrawn before the replacement is prepared.
        assert!(!fixture.ready.is_ready(&original.app_id));
        not_ready(requests.status(run_id()).await);
        release.send(()).unwrap();
        replacing.await.unwrap();
        assert!(fixture.ready.is_ready(&original.app_id));
        let calls = fixture.factory.calls();
        let old = calls[0].runtime.borrow().as_ref().unwrap().backend.clone();
        let new = calls[1].runtime.borrow().as_ref().unwrap().backend.clone();
        unavailable(start(&old).await);
        // The registry now resolves the replacement generation.
        assert!(matches!(
            requests.status(run_id()).await,
            Err(WorkflowServiceError::NotFound(_))
        ));
        bindings.close().unwrap();
        assert!(!fixture.ready.is_ready(&original.app_id));
        not_ready(requests.status(run_id()).await);
        unavailable(start(&new).await);
    })
    .await;
}

#[compio::test]
async fn readiness_and_request_mutations_publish_intents_under_the_assignment() {
    let fixture = Fixture::new();
    fixture.factory.deployed(true).await;
    let scope = scope();
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.refresh(&scope));
    exchanges.push(fixture.submission());
    exchanges.push(fixture.submission());
    peer(&fixture, exchanges, async |client| {
        let (consumer, _probe) = fixture.consumer(1);
        let bindings = fixture.bindings(client, &consumer, 1);
        bindings.reconcile().await.unwrap();
        let app = fixture.factory.calls()[0]
            .runtime
            .borrow()
            .as_ref()
            .unwrap()
            .app
            .clone();
        let leftover = app.pending_jobs(None, 16).await.unwrap();
        assert_eq!(leftover.len(), 1, "the opening left one unpublished intent");
        // Readiness marks the app once, so a predecessor's intents publish.
        let marked = compio::time::timeout(Duration::from_secs(5), bindings.marked())
            .await
            .expect("readiness must mark its app for publication");
        assert_eq!(marked, [scope.app_id.clone()].into());
        // The control: an app without a ready binding here publishes nothing.
        bindings.publish_marked([AppId::mint()].into()).await;
        assert!(fixture.submitted.borrow().is_empty());
        bindings.publish_marked(marked).await;
        assert!(app.pending_jobs(None, 16).await.unwrap().is_empty());
        let run = start(fixture.ready.backend(scope.app_id.clone()).as_ref())
            .await
            .unwrap();
        let marked = compio::time::timeout(Duration::from_secs(5), bindings.marked())
            .await
            .expect("a request-path start must mark its app");
        assert_eq!(marked, [scope.app_id.clone()].into());
        bindings.publish_marked(marked).await;
        assert!(app.pending_jobs(None, 16).await.unwrap().is_empty());
        let submitted = fixture.submitted.borrow().clone();
        assert_eq!(submitted.len(), 2);
        assert_eq!(submitted[0]["job"], json!(leftover[0]));
        for submission in &submitted {
            assert_eq!(submission["scope"], json!(scope));
        }
        let JobOperation::Advance { run_id, .. } =
            serde_json::from_value::<JobSpec>(submitted[1]["job"].clone())
                .unwrap()
                .operation
        else {
            panic!("a start publishes its first Advance");
        };
        assert_eq!(run_id.as_str(), run);
    })
    .await;
}

#[compio::test]
async fn a_settled_delivery_marks_its_app_for_publication() {
    let fixture = Fixture::new();
    let scope = scope();
    let settlement = Settlement {
        delivery: Delivery {
            job: JobSpec {
                id: JobId::mint(),
                app_id: scope.app_id.clone(),
                operation: JobOperation::Reconcile {},
                available_at: 0.try_into().unwrap(),
            },
            worker_id: fixture.worker.clone(),
            assignment_revision: scope.assignment_revision,
            attempt: 1.try_into().unwrap(),
            deadline: 0.try_into().unwrap(),
        },
        outcome: JobOutcome::Completed {},
        successors: Vec::new(),
    };
    let exchanges = vec![fixture.settlement(&settlement)];
    peer(&fixture, exchanges, async |client| {
        let (wake, marked) = publication::channel();
        let transport = HostTransport {
            client,
            settled: wake,
        };
        JobTransport::settle(&transport, &settlement).await.unwrap();
        let apps = compio::time::timeout(Duration::from_secs(5), marked.next())
            .await
            .expect("settlement must mark its app");
        assert_eq!(apps, [scope.app_id.clone()].into());
    })
    .await;
}
