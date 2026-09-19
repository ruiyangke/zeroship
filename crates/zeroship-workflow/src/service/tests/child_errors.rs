//! What a parent run is handed when its child ends without an error of its own.
//!
//! Error identity crosses the journal by name, under the `type` key the V8
//! replay bridge reconstructs a class from. These cases assert the value the
//! engine *writes*, read back off the parent's own replay journal, because that
//! row is what the bridge is handed and what a creator's `catch` matches on.

#![expect(
    clippy::future_not_send,
    reason = "child error tests own compio-local creator transactions"
)]

use super::*;
use crate::{
    operations::{RunOperation, RunState},
    service::{AppWorkflows, WorkerIdentity},
};

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

paired!(
    sqlite_a_cancelled_child_reaches_its_parent_under_the_bridge_key,
    postgres_a_cancelled_child_reaches_its_parent_under_the_bridge_key,
    cancelled_child
);
paired!(
    sqlite_a_completed_child_reaches_its_parent_without_an_error,
    postgres_a_completed_child_reaches_its_parent_without_an_error,
    completed_child
);
paired!(
    sqlite_an_expired_child_wait_reaches_its_parent_under_the_bridge_key,
    postgres_an_expired_child_wait_reaches_its_parent_under_the_bridge_key,
    expired_child_wait
);
paired!(
    sqlite_a_child_wait_with_no_timeout_leaves_its_parent_waiting,
    postgres_a_child_wait_with_no_timeout_leaves_its_parent_waiting,
    untimed_child_wait
);

/// An instant far enough behind any test clock that the wait is already past
/// due when the parent is next examined. Absolute rather than a duration, so no
/// case depends on how long the fixture took to get here.
const ELAPSED_TIMEOUT: &str = "2020-01-01T00:00:00Z";

/// Start a parent and take its first dispatch to a child call, carrying
/// `options` through to `ChildWorkflowOptions` exactly as the replay bridge
/// emits them.
async fn parent_awaiting_child(
    service: &WorkflowService,
    scope: &AppWorkflows,
    worker: &WorkerIdentity,
    options: serde_json::Value,
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
                "kind":"Child", "ordinal":0, "name":"join", "childWorkflowName":"Child",
                "options":options, "input":{}
            }])),
        )
        .await
        .unwrap();
    assert!(
        !scope.status(&parent.id).await.unwrap().state.is_terminal(),
        "the parent parks on its join"
    );
    parent.id
}

/// The child run the parent's accepted checkpoint points at.
async fn accepted_child(scope: &AppWorkflows, parent: &str) -> String {
    let mut tx = scope.service.begin().await.unwrap();
    crate::service::app::lock_app(&mut tx, scope.app_id())
        .await
        .unwrap();
    let run = crate::service::app::lock_run(&mut tx, scope.app_id(), parent)
        .await
        .unwrap();
    let child = crate::service::journal::load(&mut tx, scope.app_id(), parent, run.integer("generation").unwrap())
        .await
        .unwrap()
        .into_iter()
        .find(|step| step.kind == "child")
        .unwrap()
        .child_run_id
        .unwrap();
    tx.commit().await.unwrap();
    child
}

/// The parent's next dispatch, and with it the replayed journal row for the
/// child join. Polling is what advances the parent past its wait, so this is
/// also the assertion that the wait resolved at all.
async fn parent_join_row(
    service: &WorkflowService,
    worker: &WorkerIdentity,
    parent: &str,
) -> crate::engine::JournalStep {
    let task = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, parent);
    task.invocation
        .journal
        .into_iter()
        .find(|step| step.name == "join")
        .unwrap()
}

/// A child the platform cancelled reports no error of its own, so the parent's
/// join carries the engine's own verdict. The spelling has to land under `type`:
/// that is the key `wfDeserializeError` in the replay bridge reads to rebuild a
/// class, and `ChildCancelledError` is the name `@zeroship/workflows` matches.
/// Under any other key the bridge hands the body a bare `Error` and a creator's
/// `catch (e) { if (e instanceof ChildCancelledError) ... }` never matches.
async fn cancelled_child(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id);
    let worker = WorkerIdentity::new("cancelled-child".into()).unwrap();
    let parent = parent_awaiting_child(&service, &scope, &worker, json!({})).await;
    let child = accepted_child(&scope, &parent).await;

    // Cancellation is a request the run settles on its own next dispatch, so the
    // child is taken to the same completion the control takes it to, with the
    // request already standing. That is the one variable between them.
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, child);
    scope
        .transition(&RequestId::mint(), &child, RunOperation::Cancel)
        .await
        .unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted", "output":"child done"}])),
        )
        .await
        .unwrap();
    assert_eq!(
        scope.status(&child).await.unwrap().state,
        RunState::Cancelled
    );
    deliver_propagations(&scope).await;

    let row = parent_join_row(&service, &worker, &parent).await;
    let error = row.error.clone().expect("a cancelled child fails its parent join");
    assert_eq!(error["type"], json!("ChildCancelledError"), "{error}");
    assert_eq!(
        error["message"],
        json!("child workflow was cancelled"),
        "{error}"
    );
    assert!(row.output.is_none(), "{row:?}");
}

/// The control differing in one variable: the same parent and the same join,
/// whose child completes instead. Without it the case above would pass for an
/// engine that fails every join, whatever it named the failure.
async fn completed_child(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id);
    let worker = WorkerIdentity::new("completed-child".into()).unwrap();
    let parent = parent_awaiting_child(&service, &scope, &worker, json!({})).await;
    let child = accepted_child(&scope, &parent).await;

    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, child);
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted", "output":"child done"}])),
        )
        .await
        .unwrap();
    deliver_propagations(&scope).await;

    let row = parent_join_row(&service, &worker, &parent).await;
    assert!(row.error.is_none(), "{row:?}");
    assert_eq!(row.output, Some(json!("child done")), "{row:?}");
}

/// `ChildWorkflowOptions.timeout` bounds the join, not the child: the child is
/// still live when the bound passes, and the parent is handed the expiry of its
/// own wait. That is a different condition from the whole-run timeout and needs
/// its own name, again under `type`, or a creator cannot tell a child that was
/// cancelled from one that simply took too long.
async fn expired_child_wait(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id);
    let worker = WorkerIdentity::new("expired-child-wait".into()).unwrap();
    let parent =
        parent_awaiting_child(&service, &scope, &worker, json!({"timeout":ELAPSED_TIMEOUT})).await;
    let child = accepted_child(&scope, &parent).await;
    assert!(
        !scope.status(&child).await.unwrap().state.is_terminal(),
        "the child outlives the join it expired"
    );

    let row = parent_join_row(&service, &worker, &parent).await;
    let error = row.error.clone().expect("an expired join fails its parent");
    assert_eq!(error["type"], json!("ChildTimeoutError"), "{error}");
    assert_eq!(
        error["message"],
        json!("child workflow wait expired"),
        "{error}"
    );
    assert!(row.output.is_none(), "{row:?}");
}

/// The control differing in one variable: the same parent and the same live
/// child, joined with no timeout. The parent stays waiting, so the case above
/// rests on the bound it declared rather than on joins failing on their own.
async fn untimed_child_wait(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id);
    let worker = WorkerIdentity::new("untimed-child-wait".into()).unwrap();
    let parent = parent_awaiting_child(&service, &scope, &worker, json!({})).await;
    let child = accepted_child(&scope, &parent).await;

    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(
        task.invocation.run_id, child,
        "an untimed join leaves only the child runnable"
    );
    assert_eq!(
        scope.status(&parent).await.unwrap().state,
        RunState::Waiting
    );
}
