#![expect(
    clippy::future_not_send,
    reason = "continuation tests own compio-local creator transactions"
)]

use super::*;
use crate::{
    operations::{RestartOptions, RunOperation, RunState},
    service::{app, continuations, frontier, journal, models, AppWorkflows, WorkerIdentity},
};
use std::collections::BTreeMap;
use zeroship_data_orm::orm::FindOptions;

mod provenance;

macro_rules! paired {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            let store = sqlite_store(&directory.path().join("workflow.sqlite")).await;
            Box::pin($contract(Rc::new(store))).await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

mod corruption;
mod references;

paired!(
    sqlite_continuation_preserves_pending_keyed_join_targets,
    postgres_continuation_preserves_pending_keyed_join_targets,
    pending_targets
);
paired!(
    sqlite_restart_distinguishes_current_and_historical_continuation_members,
    postgres_restart_distinguishes_current_and_historical_continuation_members,
    restart_heads
);
paired!(
    sqlite_continued_child_inherits_cancellation_and_compensation,
    postgres_continued_child_inherits_cancellation_and_compensation,
    cancellation
);
paired!(
    sqlite_continuation_rollback_and_retired_authority_preserve_chain,
    postgres_continuation_rollback_and_retired_authority_preserve_chain,
    rollback
);

async fn parent(
    service: &WorkflowService,
    scope: &AppWorkflows,
    worker: &WorkerIdentity,
    key: Option<&str>,
) -> String {
    let parent = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, parent.id);
    service
        .complete(
            worker,
            &task.id,
            &task.token,
            execution(json!([{
                "kind":"Child", "ordinal":0, "name":"child", "childWorkflowName":"Child",
                "options":{"key":key, "cascade":true}, "input":{}
            }])),
        )
        .await
        .unwrap();
    parent.id
}

async fn accepted(scope: &AppWorkflows, parent: &str) -> (String, String) {
    let mut tx = scope.service.begin().await.unwrap();
    app::lock_app(&mut tx, scope.app_id()).await.unwrap();
    let row = tx
        .database()
        .entity::<models::steps::Entity>()
        .unwrap()
        .find::<models::StoredStep>(
            models::steps::app_id
                .eq(scope.app_id().as_str())
                .unwrap()
                .and(models::steps::run_id.eq(parent).unwrap())
                .and(models::steps::generation.eq(0_i64).unwrap()),
            FindOptions {
                limit: Some(1),
                ..FindOptions::default()
            },
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    let step = journal::read_checkpoint(&tx, scope.app_id(), &row)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (step.child_run_id.unwrap(), row.child_member_id.unwrap())
}

async fn transition(scope: &AppWorkflows, run: &str, operation: RunOperation) {
    scope
        .transition(&RequestId::mint(), run, operation)
        .await
        .unwrap();
}

async fn continue_run(
    service: &WorkflowService,
    scope: &AppWorkflows,
    worker: &WorkerIdentity,
    expected: &str,
) -> String {
    let task = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, expected);
    service
        .complete(
            worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"ContinueAsNew", "input":"next"}])),
        )
        .await
        .unwrap();
    scope.status(expected).await.unwrap().output.unwrap()["continuedAsNew"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn finish_run(
    service: &WorkflowService,
    worker: &WorkerIdentity,
    expected: &str,
    output: &str,
) {
    let task = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, expected);
    service
        .complete(
            worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted", "output":output}])),
        )
        .await
        .unwrap();
}

async fn parent_state(
    service: &WorkflowService,
    app: &AppId,
    parent: &str,
) -> BTreeMap<&'static str, Vec<zeroship_data_orm::Value>> {
    let tx = service.begin().await.unwrap();
    let mut snapshot = BTreeMap::new();
    for table in ["steps", "waits", "job_publications"] {
        snapshot.insert(
            table,
            journal_rows(&tx, table, json!({"app_id":app.as_str(), "run_id":parent}))
                .await
                .into_iter()
                .map(|row| row.0)
                .collect(),
        );
    }
    tx.commit().await.unwrap();
    snapshot
}

async fn pending_targets(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id.clone());
    let worker = WorkerIdentity::new("stable-pending-joins".into()).unwrap();
    let owner = parent(&service, &scope, &worker, Some("shared")).await;
    let (child, identity) = accepted(&scope, &owner).await;
    transition(&scope, &child, RunOperation::Pause).await;
    let joiner = parent(&service, &scope, &worker, Some("shared")).await;
    assert_eq!(
        accepted(&scope, &joiner).await,
        (child.clone(), identity.clone())
    );
    transition(&scope, &owner, RunOperation::Pause).await;
    let saved_owner = parent_state(&service, &app_id, &owner).await;
    let saved_joiner = parent_state(&service, &app_id, &joiner).await;
    transition(&scope, &child, RunOperation::Resume).await;
    let middle = continue_run(&service, &scope, &worker, &child).await;
    let head = continue_run(&service, &scope, &worker, &middle).await;
    assert_eq!(parent_state(&service, &app_id, &owner).await, saved_owner);
    assert_eq!(parent_state(&service, &app_id, &joiner).await, saved_joiner);
    assert_eq!(accepted(&scope, &owner).await, (child, identity));
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &app_id).await.unwrap();
    let run = app::lock_run(&mut tx, &app_id, &head).await.unwrap();
    assert_eq!(
        run.optional_text("parent_id").unwrap().as_deref(),
        Some(owner.as_str())
    );
    assert_eq!(run.integer("cascade").unwrap(), 1);
    assert_eq!(run.integer("depth").unwrap(), 1);
    tx.commit().await.unwrap();
    finish_run(&service, &worker, &head, "terminal head").await;
    let resumed = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(resumed.invocation.run_id, joiner);
    assert_eq!(
        resumed.invocation.journal[0].output,
        Some(json!("terminal head"))
    );
    assert_eq!(
        resumed.invocation.journal[0].child_run_id.as_deref(),
        Some(head.as_str())
    );
    assert_eq!(scope.status(&owner).await.unwrap().state, RunState::Paused);
}

async fn restart_heads(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id.clone());
    let worker = WorkerIdentity::new("restart-heads".into()).unwrap();
    let parent = parent(&service, &scope, &worker, None).await;
    let (source, accepted_id) = accepted(&scope, &parent).await;
    transition(&scope, &parent, RunOperation::Pause).await;
    let next = continue_run(&service, &scope, &worker, &source).await;
    transition(&scope, &next, RunOperation::Pause).await;
    let saved = parent_state(&service, &app_id, &parent).await;
    scope
        .restart(&RequestId::mint(), &source, RestartOptions::default())
        .await
        .unwrap();
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &app_id).await.unwrap();
    let original = continuations::by_id(&tx, &app_id, &accepted_id)
        .await
        .unwrap();
    let restarted = continuations::member(&tx, &app_id, &source, 1)
        .await
        .unwrap();
    assert_ne!(original.head_id, restarted.head_id);
    assert!(!original.is_current);
    tx.commit().await.unwrap();
    finish_run(&service, &worker, &source, "independent historical restart").await;
    assert_eq!(parent_state(&service, &app_id, &parent).await, saved);
    scope
        .restart(&RequestId::mint(), &next, RestartOptions::default())
        .await
        .unwrap();
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &app_id).await.unwrap();
    let current = continuations::member(&tx, &app_id, &next, 1).await.unwrap();
    assert_eq!(current.head_id, original.head_id);
    assert!(current.is_current);
    tx.commit().await.unwrap();
    finish_run(&service, &worker, &next, "restarted head").await;
    transition(&scope, &parent, RunOperation::Resume).await;
    let resumed = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(resumed.invocation.run_id, parent);
    assert_eq!(
        resumed.invocation.journal[0].output,
        Some(json!("restarted head"))
    );
}

async fn cancellation(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id);
    let worker = WorkerIdentity::new("continued-cancellation".into()).unwrap();
    let owner = parent(&service, &scope, &worker, Some("owned")).await;
    let (child, _) = accepted(&scope, &owner).await;
    let head = continue_run(&service, &scope, &worker, &child).await;
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, head);
    service.complete(&worker, &task.id, &task.token, execution(json!([
        {"kind":"StepCompleted", "ordinal":0, "name":"undo", "compensable":true, "output":"effect"},
        {"kind":"Wait", "ordinal":1, "name":"wait", "signalType":"release"}
    ]))).await.unwrap();
    transition(&scope, &owner, RunOperation::Cancel).await;
    let compensation = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(compensation.invocation.run_id, head);
    assert_eq!(compensation.invocation.phase, "compensating");
    service
        .complete(
            &worker,
            &compensation.id,
            &compensation.token,
            execution(json!([{"kind":"CompensationCompleted", "ordinal":0, "name":"undo"}])),
        )
        .await
        .unwrap();
    assert_eq!(
        scope.status(&owner).await.unwrap().state,
        RunState::Cancelled
    );
    assert_eq!(
        scope.status(&head).await.unwrap().state,
        RunState::Cancelled
    );
    assert_eq!(
        scope.status(&child).await.unwrap().state,
        RunState::Completed
    );
}

async fn all_state(
    service: &WorkflowService,
    app: &AppId,
) -> BTreeMap<&'static str, Vec<zeroship_data_orm::Value>> {
    let tx = service.begin().await.unwrap();
    let mut result = BTreeMap::new();
    for table in [
        "runs",
        "generations",
        "continuation_heads",
        "continuation_members",
        "job_publications",
        "outbox",
        "steps",
        "waits",
        "tasks",
    ] {
        let mut rows = journal_rows(&tx, table, json!({"app_id":app.as_str()})).await;
        rows.sort_by_key(|row| row.text("id").unwrap());
        result.insert(table, rows.into_iter().map(|row| row.0).collect());
    }
    tx.commit().await.unwrap();
    result
}

async fn rollback(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id.clone());
    let worker = WorkerIdentity::new("continuation-rollback".into()).unwrap();
    let owner = parent(&service, &scope, &worker, None).await;
    let (child, _) = accepted(&scope, &owner).await;
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, child);
    let before = all_state(&service, &app_id).await;
    for retire in [false, true] {
        let scope = service.fixture_app(app_id.clone());
        let mut tx = scope.service.begin().await.unwrap();
        let policy = app::lock_app(&mut tx, &app_id).await.unwrap();
        let run = app::lock_run(&mut tx, &app_id, &child).await.unwrap();
        let now = tx.now().await.unwrap();
        Box::pin(frontier::apply(
            &mut tx,
            &app_id,
            &run,
            &policy,
            execution(json!([{"kind":"ContinueAsNew", "input":"uncommitted"}])),
            now,
        ))
        .await
        .unwrap();
        if retire {
            service
                .policies
                .current_binding(&app_id)
                .unwrap()
                .revoke()
                .unwrap();
            assert!(matches!(
                tx.commit().await,
                Err(WorkflowServiceError::Unavailable(_))
            ));
            service
                .policies
                .fixture_install(&app_id, leased_policy(1, AppPolicy::default()))
                .unwrap();
        } else {
            drop(tx);
        }
        assert_eq!(all_state(&service, &app_id).await, before);
    }
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"ContinueAsNew", "input":"committed"}])),
        )
        .await
        .unwrap();
    let scope = service.fixture_app(app_id);
    let successor = scope.status(&child).await.unwrap().output.unwrap()["continuedAsNew"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(successor, child);
    assert_eq!(
        scope.status(&successor).await.unwrap().state,
        RunState::Queued
    );
}
