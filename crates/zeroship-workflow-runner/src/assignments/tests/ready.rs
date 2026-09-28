//! Request-path resolution of the app backends a host has published.

use super::*;
use zeroship_core::{service_identity::endpoints, workflow_coordination::RunFailure};
use zeroship_workflow::{backend::WorkflowBackend, operations::StartOptions};

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

