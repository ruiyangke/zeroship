use super::*;
use futures::{channel::oneshot, future::Either};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    time::Duration,
};
use zeroship_workflow_runner::ExecutionGuard;

#[path = "../../../../tests/fixtures/workflow_deployments.rs"]
mod deployment_fixture;
mod fixture;
use fixture::{execute, install, Fixture};

#[compio::test]
async fn unknown_assignment_and_wrong_policy_are_refused_before_creator_io() {
    let fixture = Fixture::new().await;
    let factory = fixture.factory();
    let unknown = AssignedScope {
        app_id: AppId::mint(),
        assignment_revision: fixture.scope.assignment_revision,
    };
    assert!(matches!(
        factory.open(&unknown, &fixture.policy, fixture.ingress()).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(fixture.provider.calls().is_empty());
    let foreign_registry = Arc::new(HostPolicies::default());
    let foreign_policy = install(&foreign_registry, fixture.scope.app_id.clone());
    assert!(matches!(
        factory.open(&fixture.scope, &foreign_policy, fixture.ingress()).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(fixture.provider.calls().is_empty());
    let unknown_policy = install(&fixture.policies, unknown.app_id.clone());
    assert!(matches!(
        factory.open(&unknown, &unknown_policy, fixture.ingress()).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(fixture.provider.calls(), vec![unknown]);
    assert_eq!(fixture.contexts.calls.get(), 0);
}

#[compio::test]
async fn initial_context_must_name_the_assigned_app() {
    let fixture = Fixture::new().await;
    fixture.contexts.current.borrow_mut().app = AppId::mint();
    assert!(matches!(
        fixture
            .factory()
            .open(&fixture.scope, &fixture.policy, fixture.ingress())
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(fixture.contexts.calls.get(), 1);
}

#[compio::test]
async fn policy_replacement_cancels_pending_resource_resolution_without_creator_io() {

    let fixture = Fixture::new().await;
    let factory = fixture.factory();
    let (observed, release) = fixture.provider.gate();
    let mut opening = Box::pin(factory.open(&fixture.scope, &fixture.policy, fixture.ingress()));
    assert!(matches!(
        futures::future::select(observed, opening.as_mut()).await,
        Either::Left((Ok(()), _))
    ));
    let replacement = install(&fixture.policies, fixture.scope.app_id.clone());
    assert!(matches!(
        opening.await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(fixture.provider.dropped());
    assert!(
        release.send(()).is_err(),
        "replacement must drop the pending provider future"
    );
    assert!(fixture.policy.begin_refresh().is_err());
    assert!(replacement.begin_refresh().is_ok());
    assert_eq!(fixture.contexts.calls.get(), 0);
}

/// The severed assembly runs creator code: the factory's loader resolves the
/// pinned deployment ACROSS the transport, loads its bytes from the artifact
/// store this host holds, and the executor it returns produces a frontier.
///
/// The journal is the service's, so nothing here starts a run or settles one --
/// those are bound where the service answers. What is bound here is the wiring:
/// one resolution crosses, the bytes do not, and the executable that reaches V8
/// is the one the resolution named.
#[compio::test]
async fn factory_executes_a_pinned_frontier_from_a_crossed_resolution() {
    zeroship_runtime::init_v8();
    let fixture = Fixture::new().await;
    let deploy_hash = fixture
        .publish(
            r"
        export class Example {
            async run(trigger, step) {
                const seen = await step.run('seen', {}, () => trigger.input.value);
                return seen;
            }
        }
    ",
        )
        .await;
    // The pin the service answers with. The hash is what the loader verifies the
    // artifact bytes against, so a peer naming another deployment cannot have its
    // bytes loaded under this one's identity.
    let peer = fixture
        .serving(json!({
            "deployId": zeroship_core::typed_id::generate("dep"),
            "deployHash": deploy_hash,
            "availabilityEpoch": 1,
            "admissionGeneration": 1,
        }))
        .await;
    let runtime = fixture
        .factory_through(peer.client.clone())
        .open(&fixture.scope, &fixture.policy, fixture.ingress())
        .await
        .unwrap();
    assert_eq!(runtime.backend.scope(), &fixture.scope);
    let assignment = fixture.assignment(&deploy_hash);
    let execution = execute(&runtime, &assignment).await.unwrap();
    assert_eq!(
        peer.served().await,
        1,
        "one resolution crosses per execution, and the bytes never do"
    );
    // The creator step ran and its value reached the frontier.
    let outcomes = serde_json::to_value(&execution).unwrap();
    assert!(
        format!("{outcomes}").contains("creator-owned-input"),
        "{outcomes}"
    );
    assert_eq!(fixture.provider.calls(), vec![fixture.scope.clone()]);
    assert!(
        fixture.contexts.calls.get() > 1,
        "task loads resolve current runtime metadata again"
    );
}

/// A context that moves an installed creator to another app is refused at TASK
/// LOAD, not only at assembly.
///
/// The loader resolves fresh metadata for every execution, so a provider that
/// starts answering for another tenant must be refused there. The control is the
/// same assignment executing again once the app is restored: without it, a
/// refusal for any other reason would also pass.
#[compio::test]
async fn dynamic_context_cannot_move_an_installed_creator_to_another_app() {
    zeroship_runtime::init_v8();
    let fixture = Fixture::new().await;
    // A workflow whose value stays INLINE, so the only call this execution makes
    // is the executable resolution. A returned object would reserve a payload
    // too, and this test is about the context check rather than the payload seam.
    let deploy_hash = fixture
        .publish(
            r"
        export class Example {
            async run(trigger, step) {
                return await step.run('bound', {}, () => 'bound');
            }
        }
    ",
        )
        .await;
    let peer = fixture
        .serving(json!({
            "deployId": zeroship_core::typed_id::generate("dep"),
            "deployHash": deploy_hash,
            "availabilityEpoch": 1,
            "admissionGeneration": 1,
        }))
        .await;
    let runtime = fixture
        .factory_through(peer.client.clone())
        .open(&fixture.scope, &fixture.policy, fixture.ingress())
        .await
        .unwrap();
    let assignment = fixture.assignment(&deploy_hash);
    let original = fixture.contexts.current.borrow().app.clone();
    fixture.contexts.current.borrow_mut().app = AppId::mint();
    assert!(matches!(
        execute(&runtime, &assignment).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    fixture.contexts.current.borrow_mut().app = original;
    execute(&runtime, &assignment).await.unwrap();
    assert!(
        peer.served().await > 0,
        "the accepted execution must have resolved its pin across the transport"
    );
}
