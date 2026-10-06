use super::*;
use futures::{channel::oneshot, future::Either};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    time::Duration,
};
use zeroship_workflow_runner::{
    prepared::{PreparedApps, PreparedOptions},
    ExecutionGuard,
};

use crate::workflow_fixtures::deployment as deployment_fixture;

mod fixture;
use fixture::{execute, Fixture};

/// An app the provider does not authorize is refused there, and nothing about
/// it reaches creator code: no context is resolved and no executor is built.
///
/// The control is the fixture's own app through the same factory, which does
/// resolve a context; without it a factory that refused every app would pass.
#[compio::test]
async fn an_app_the_provider_refuses_is_refused_before_creator_io() {
    let fixture = Fixture::new().await;
    let factory = fixture.factory();
    let unknown = AppId::mint();
    assert!(matches!(
        factory.open(&unknown).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(fixture.provider.calls(), vec![unknown]);
    assert_eq!(fixture.contexts.calls.get(), 0);

    factory
        .open(&fixture.app)
        .await
        .expect("the provider's own app is prepared");
    assert_eq!(fixture.contexts.calls.get(), 1);
}

#[compio::test]
async fn initial_context_must_name_the_claimed_app() {
    let fixture = Fixture::new().await;
    fixture.contexts.current.borrow_mut().app = AppId::mint();
    assert!(matches!(
        fixture.factory().open(&fixture.app).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(fixture.contexts.calls.get(), 1);
}

/// The prepared app holds the residency its resources were resolved under, and
/// releases it when it is dropped.
///
/// That residency is what keeps the app's key, bindings and environment
/// supplied while an execution runs from the prepared app, so a factory that
/// built the runtime under any other guard would leave them withdrawable
/// mid-execution.
#[compio::test]
async fn the_prepared_app_holds_the_residency_its_resources_were_resolved_under() {
    let fixture = Fixture::new().await;
    let resolved = fixture.provider.resources().residency;
    let before = Rc::strong_count(&resolved);
    let runtime = fixture
        .factory()
        .open(&fixture.app)
        .await
        .expect("the provider's own app is prepared");
    assert!(
        Rc::ptr_eq(&runtime.residency, &resolved),
        "the prepared app holds the provider's residency, not one of its own"
    );
    assert_eq!(Rc::strong_count(&resolved), before + 1);
    drop(runtime);
    assert_eq!(
        Rc::strong_count(&resolved),
        before,
        "dropping the prepared app releases its hold"
    );
}

/// A preparation is bounded by the delivery that asked for it, and a bound
/// that runs out drops the pending resolution rather than finishing it later.
///
/// The provider is held open by a gate nothing releases, so the delivery's
/// remaining lease is the only thing that can end the wait. The control is the
/// same preparation with the gate released inside the bound.
#[compio::test]
async fn a_preparation_past_its_delivery_bound_drops_the_pending_resolution() {
    let fixture = Fixture::new().await;
    let prepared = PreparedApps::new(
        fixture.factory(),
        Rc::new(|_: &AppId| true),
        PreparedOptions {
            capacity: 1,
            operation_timeout: Duration::from_mins(10),
        },
    )
    .unwrap();
    let (observed, release) = fixture.provider.gate();
    let mut preparing = Box::pin(prepared.get_or_prepare(&fixture.app, Duration::from_millis(50)));
    assert!(matches!(
        futures::future::select(observed, preparing.as_mut()).await,
        Either::Left((Ok(()), _))
    ));
    // The cache's own bound is far longer, so only the delivery's can end
    // the wait inside this window.
    let outcome = compio::time::timeout(Duration::from_secs(5), preparing)
        .await
        .expect("the delivery's bound ends the preparation, not the cache's");
    assert!(matches!(outcome, Err(WorkflowServiceError::Timeout)));
    assert!(fixture.provider.dropped());
    assert!(
        release.send(()).is_err(),
        "the bound must drop the pending provider future"
    );
    assert_eq!(fixture.contexts.calls.get(), 0);

    // THE CONTROL: released inside the bound, the same preparation completes.
    let (observed, release) = fixture.provider.gate();
    let mut preparing = Box::pin(prepared.get_or_prepare(&fixture.app, Duration::from_secs(5)));
    assert!(matches!(
        futures::future::select(observed, preparing.as_mut()).await,
        Either::Left((Ok(()), _))
    ));
    release.send(()).expect("the pending resolution is still waiting");
    preparing.await.expect("a resolution inside its bound prepares the app");
    assert_eq!(fixture.contexts.calls.get(), 1);
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
        .open(&fixture.app)
        .await
        .unwrap();
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
    assert_eq!(fixture.provider.calls(), vec![fixture.app.clone()]);
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
        .open(&fixture.app)
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
