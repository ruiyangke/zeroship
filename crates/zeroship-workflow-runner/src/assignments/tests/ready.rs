//! Request-path publication of prepared app backends and their intents.

use super::*;
use crate::publication::{self, HostTransport};
use zeroship_core::{service_identity::endpoints, workflow_coordination::RunFailure};
use zeroship_workflow::{backend::WorkflowBackend, operations::StartOptions};
use zeroship_core::workflow_jobs::{
    Delivery, JobId, JobOperation, JobOutcome, JobSpec, Settlement,
};

async fn start(backend: &dyn WorkflowBackend) -> Result<String, WorkflowServiceError> {
    backend
        .start("Example".into(), serde_json::Value::Null, StartOptions::default())
        .await
        .map(|run| run.id)
}

#[compio::test]
async fn backend_is_published_only_after_preparation_and_withdrawn_on_removal() {
    let fixture = Fixture::new();
    let scope = scope();
    let (observed, release) = fixture.factory.gate(&scope.app_id);
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.establish(&scope));
    exchanges.push(fixture.page(None, &[]));
    exchanges.push(fixture.run_refusal(
        endpoints::WORKFLOW_RUN_STATUS,
        &scope,
        &RunFailure::NotFound {
            message: "workflow run not found".into(),
        },
    ));
    exchanges.push(fixture.run_refusal(
        endpoints::WORKFLOW_RUN_START,
        &scope,
        &RunFailure::PermissionDenied {},
    ));
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
        // A client cloned before removal keeps its retired generation, and asks
        // the service under it rather than being answered locally.
        assert_eq!(retained.scope(), &scope);
        assert!(matches!(
            start(&retained).await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
    })
    .await;
}

/// A generation this process is not permitted to serve is refused, never
/// published, and given back: the host releases the placement with the closed
/// refused reason so the manager places the app on another instance and never
/// offers this pair to this one again.
#[compio::test]
async fn a_request_backend_from_another_generation_is_refused_and_never_published() {
    let fixture = Fixture::new();
    fixture.factory.foreign_backend();
    let scope = scope();
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.establish(&scope));
    exchanges.push(fixture.release());
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
        let released = fixture.submitted.borrow().clone();
        assert_eq!(released.len(), 1, "{released:?}");
        assert_eq!(released[0]["appId"], json!(scope.app_id));
        assert_eq!(
            released[0]["assignmentRevision"],
            json!(scope.assignment_revision)
        );
        assert_eq!(released[0]["reason"], json!("refused"));
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
    exchanges.extend(fixture.establish(&original));
    exchanges.extend(fixture.scan(std::slice::from_ref(&replacement)));
    exchanges.extend(fixture.establish(&replacement));
    // Three creator calls cross, each matched by the placement it names. The
    // service decides what a call is admitted under, so a refusal is scripted
    // here and the PAIRING of reason to code is bound where a real coordinator
    // answers -- `a_moved_assignment_revision_conflicts_rather_than_denying` in
    // `zeroship-workflow-server`. What this test settles is that the runner
    // still sends a retired generation's call under its OWN placement, and
    // surfaces the answer rather than inventing one.
    exchanges.push(fixture.run_refusal(
        endpoints::WORKFLOW_RUN_START,
        &original,
        &RunFailure::Conflict {
            message: "workflow placement is no longer current".into(),
        },
    ));
    exchanges.push(fixture.run_refusal(
        endpoints::WORKFLOW_RUN_STATUS,
        &replacement,
        &RunFailure::NotFound {
            message: "workflow run not found".into(),
        },
    ));
    exchanges.push(fixture.run_refusal(
        endpoints::WORKFLOW_RUN_STATUS,
        &replacement,
        &RunFailure::NotFound {
            message: "workflow run not found".into(),
        },
    ));
    exchanges.push(fixture.run_refusal(
        endpoints::WORKFLOW_RUN_START,
        &replacement,
        &RunFailure::PermissionDenied {},
    ));
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
        assert_eq!(old.scope(), &original);
        assert_eq!(new.scope(), &replacement);
        // The retired generation still calls under its own placement, and the
        // service's refusal of that placement is what the creator is told. A
        // retry is the honest instruction: the registry below already resolves
        // the generation that now holds.
        assert!(matches!(
            start(&old).await,
            Err(WorkflowServiceError::Conflict(_))
        ));
        // The registry now resolves the replacement generation.
        assert!(matches!(
            requests.status(run_id()).await,
            Err(WorkflowServiceError::NotFound(_))
        ));
        // A retired generation cannot withdraw its replacement: a late
        // retirement of the old placement leaves the published one reachable.
        fixture.ready.retire(&original);
        assert!(fixture.ready.is_ready(&original.app_id));
        assert!(matches!(
            requests.status(run_id()).await,
            Err(WorkflowServiceError::NotFound(_))
        ));
        bindings.close().unwrap();
        assert!(!fixture.ready.is_ready(&original.app_id));
        not_ready(requests.status(run_id()).await);
        // A handle retained past closure is not silenced locally either: the
        // placement it names was released, so the far end refuses it.
        assert!(matches!(
            start(&new).await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
    })
    .await;
}

/// Readiness publishes a predecessor's intents; a request-path start does not.
///
/// The first half is this host's own: an app becoming ready marks it once, so
/// intents a previous process committed and never published are submitted under
/// the current assignment.
///
/// The second half is what the severance changed. A request-path start now
/// commits in the SERVICE's journal, so there is no local intent for this host to
/// publish and nothing marks it. The negative is asserted against the positive
/// above it, which is what shows the channel was working and simply had nothing
/// to carry.
#[compio::test]
async fn readiness_publishes_predecessor_intents_and_a_crossed_start_marks_nothing() {
    let fixture = Fixture::new();
    fixture.factory.deployed(true).await;
    let scope = scope();
    let mut exchanges = fixture.scan(std::slice::from_ref(&scope));
    exchanges.extend(fixture.establish(&scope));
    exchanges.push(fixture.submission());
    exchanges.push(fixture.run_call(
        endpoints::WORKFLOW_RUN_START,
        &scope,
        200,
        json!({"id": run_id(), "state": "queued"}),
    ));
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
        let submitted = fixture.submitted.borrow().clone();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0]["job"], json!(leftover[0]));
        assert_eq!(submitted[0]["scope"], json!(scope));
        // The request path crosses, so the run it starts exists in the service's
        // journal and this one gains no intent to publish.
        start(fixture.ready.backend(scope.app_id.clone()).as_ref())
            .await
            .unwrap();
        assert!(
            compio::time::timeout(Duration::from_secs(1), bindings.marked())
                .await
                .is_err(),
            "a crossed start must leave this host nothing to publish"
        );
        assert!(app.pending_jobs(None, 16).await.unwrap().is_empty());
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
