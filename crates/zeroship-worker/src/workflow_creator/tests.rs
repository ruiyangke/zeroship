use super::*;
use futures::{channel::oneshot, future::Either};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    time::{Duration, Instant},
};
use zeroship_core::workflow_jobs::{
    Delivery, DeploymentId, JobId, JobLease, JobOperation, JobOutcome, JobSpec,
};
use zeroship_workflow::{
    operations::{RunState, StartOptions},
    service::{
        delivery::{DeliveredTask, JobAcceptance},
        runner::ExecutionGuard,
        AppPolicy, RequestId,
    },
};

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
    fixture.assert_storage_unopened();
}

#[compio::test]
async fn foreign_storage_app_is_refused_before_context_or_database_open() {
    use zeroship_data_orm::binding::DbBinding;

    let fixture = Fixture::new().await;
    let mut resources = fixture.provider.resources();
    let other = AppId::mint();
    resources.storage.binding = DbBinding::new(
        other.as_str(),
        "foreign-fixture",
        SchemaName::new(other.as_str()).unwrap(),
    );
    fixture.provider.replace(resources);
    assert!(matches!(
        fixture
            .factory()
            .open(&fixture.scope, &fixture.policy, fixture.ingress())
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(fixture.provider.calls(), vec![fixture.scope.clone()]);
    assert_eq!(fixture.contexts.calls.get(), 0);
    fixture.assert_storage_unopened();
}

#[compio::test]
async fn initial_context_must_match_the_storage_app_and_physical_schema() {
    for wrong_app in [false, true] {
        let fixture = Fixture::new().await;
        let other = AppId::mint();
        if wrong_app {
            fixture.contexts.current.borrow_mut().app = other;
        } else {
            fixture.contexts.current.borrow_mut().schema = SchemaName::new(other.as_str()).unwrap();
        }
        assert!(matches!(
            fixture
                .factory()
                .open(&fixture.scope, &fixture.policy, fixture.ingress())
                .await,
            Err(WorkflowServiceError::PermissionDenied)
        ));
        assert_eq!(fixture.contexts.calls.get(), 1);
        fixture.assert_storage_unopened();
    }
}

#[compio::test]
async fn factory_verifies_missing_journal_without_provisioning_it() {
    let fixture = Fixture::new().await;
    let factory = fixture.factory();
    assert!(factory.open(&fixture.scope, &fixture.policy, fixture.ingress()).await.is_err());
    let store = fixture.provider.resources().storage.open().await.unwrap();
    assert!(
        store.verify().await.is_err(),
        "failed assembly must not install the workflow schema"
    );
    fixture.provision().await;
    let runtime = factory.open(&fixture.scope, &fixture.policy, fixture.ingress()).await.unwrap();
    assert_eq!(runtime.app.app_id(), &fixture.scope.app_id);
    assert!(runtime.app.pending_jobs(None, 1).await.unwrap().is_empty());
}

#[compio::test]
async fn policy_replacement_cancels_pending_resource_resolution_without_storage_io() {
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
    fixture.assert_storage_unopened();
}

#[compio::test]
async fn factory_executes_delivered_v8_frontiers_with_its_creator_artifact_and_payloads() {
    zeroship_runtime::init_v8();
    let fixture = Fixture::new().await;
    fixture.provision().await;
    let ingress = fixture.ingress();
    let runtime = fixture
        .factory()
        .open(&fixture.scope, &fixture.policy, ingress.clone())
        .await
        .unwrap();
    let deployment = fixture.activate(&runtime, r"
        export class Example {
            async run(trigger, step) {
                const saved = await step.run('saved', {output:{as:'blob', contentType:'application/json'}}, () => ({value:trigger.input.value}));
                return (await saved.json()).value;
            }
        }
    ").await;
    let input = "creator-owned-payload".repeat(8);
    let started = runtime
        .app
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input: json!({"value":input}),
                ..StartOptions::default()
            },
        )
        .await
        .unwrap();
    // The factory attached the placement's establishment: the lease held no
    // epoch, so the refused start obtained one and was accepted on retry.
    assert_eq!(*ingress.requested.borrow(), vec![None]);
    assert_eq!(ingress.accepted.get(), 1);
    let mut finished = false;
    for _ in 0..8 {
        let lease = fixture.next(&runtime.app, &started.id).await;
        assert_eq!(lease.delivery.job.deployment_id(), Some(&deployment));
        let JobAcceptance::Execute(task) = runtime.app.accept_job(&lease).await.unwrap() else {
            panic!("exact published Advance must be executable");
        };
        let outcome = execute(&runtime, &task).await.unwrap();
        let receipt = runtime
            .app
            .complete_job(&task, &lease, outcome)
            .await
            .unwrap();
        assert_eq!(
            runtime.app.job_receipt(&lease.delivery.job).await.unwrap(),
            Some(receipt.clone())
        );
        assert!(
            matches!(runtime.app.accept_job(&lease).await.unwrap(), JobAcceptance::Settled(replayed) if replayed == receipt)
        );
        let status = runtime.app.status(&started.id).await.unwrap();
        if status.state == RunState::Completed {
            assert_eq!(status.output, Some(json!(input)));
            finished = true;
            break;
        }
        assert!(matches!(status.state, RunState::Queued | RunState::Running));
    }
    assert!(
        finished,
        "bounded delivered frontiers must finish the workflow"
    );
    let saved = runtime
        .app
        .read_step_output(&started.id, "saved", 0)
        .await
        .unwrap()
        .into_bytes(4096)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&saved).unwrap(),
        json!({"value":input})
    );
    assert!(
        fixture::contains_file(&fixture.directory.path().join("objects")),
        "the returned executor must stage referenced outputs in the supplied object store"
    );
    assert_eq!(fixture.provider.calls(), vec![fixture.scope.clone()]);
    assert!(
        fixture.contexts.calls.get() > 1,
        "task loads resolve current runtime metadata again"
    );
}

#[compio::test]
async fn dynamic_context_cannot_move_an_installed_creator_to_another_schema() {
    zeroship_runtime::init_v8();
    let fixture = Fixture::new().await;
    fixture.provision().await;
    let runtime = fixture
        .factory()
        .open(&fixture.scope, &fixture.policy, fixture.ingress())
        .await
        .unwrap();
    fixture
        .activate(
            &runtime,
            "export class Example { run() { return 'bound'; } }",
        )
        .await;
    let started = runtime
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let lease = fixture.next(&runtime.app, &started.id).await;
    let JobAcceptance::Execute(task) = runtime.app.accept_job(&lease).await.unwrap() else {
        panic!("published job must be claimable");
    };
    let original = fixture.contexts.current.borrow().schema.clone();
    fixture.contexts.current.borrow_mut().schema = SchemaName::new(AppId::mint().as_str()).unwrap();
    assert!(matches!(
        execute(&runtime, &task).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(runtime
        .app
        .job_receipt(&lease.delivery.job)
        .await
        .unwrap()
        .is_none());
    fixture.contexts.current.borrow_mut().schema = original;
    let execution = execute(&runtime, &task).await.unwrap();
    runtime
        .app
        .complete_job(&task, &lease, execution)
        .await
        .unwrap();
    assert_eq!(
        runtime.app.status(&started.id).await.unwrap().output,
        Some(json!("bound"))
    );
}
