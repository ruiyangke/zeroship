//! What a run generation's journal admits of its children's outputs.
//!
//! Each completed child's output is an object its parent's replay reads back
//! in full, so `AppPolicy::max_child_output_bytes` bounds their sum where the
//! journal grows: the child whose output would carry the sum past it is
//! recorded failed with `LimitExceededError`, the class the replay bridge
//! rebuilds and a creator's `catch` matches, instead of being attached.

#![expect(
    clippy::future_not_send,
    reason = "child output tests own compio-local creator transactions"
)]

use super::*;
use crate::service::{AppWorkflows, WorkerIdentity};

macro_rules! paired {
    ($sqlite:ident, $postgres:ident, $contract:ident, $limit:expr) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            let store = sqlite_store(&directory.path().join("workflow.sqlite")).await;
            Box::pin($contract(Rc::new(store), $limit)).await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()), $limit)).await;
        }
    };
}

/// Two child outputs of this many bytes each: one fits under [`OVER`], both do
/// not, and both fit exactly at [`BOTH`].
const OUTPUT: &[u8] = br#""xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx""#;
const BOTH: usize = 2 * OUTPUT.len();
const OVER: usize = BOTH - 1;

paired!(
    sqlite_children_completing_together_are_held_to_the_bound_in_ordinal_order,
    postgres_children_completing_together_are_held_to_the_bound_in_ordinal_order,
    together,
    OVER
);
paired!(
    sqlite_children_completing_together_attach_when_the_bound_admits_both,
    postgres_children_completing_together_attach_when_the_bound_admits_both,
    together,
    BOTH
);
paired!(
    sqlite_a_later_child_counts_the_outputs_already_attached,
    postgres_a_later_child_counts_the_outputs_already_attached,
    one_after_another,
    OVER
);
paired!(
    sqlite_a_later_child_attaches_when_the_bound_admits_both,
    postgres_a_later_child_attaches_when_the_bound_admits_both,
    one_after_another,
    BOTH
);
paired!(
    sqlite_continue_as_new_starts_the_child_output_total_from_nothing,
    postgres_continue_as_new_starts_the_child_output_total_from_nothing,
    continued,
    OVER
);
paired!(
    sqlite_restart_counts_the_retained_child_output_prefix,
    postgres_restart_counts_the_retained_child_output_prefix,
    restarted,
    OVER
);

/// A service whose app bounds child outputs at `limit`, and a parent run's id.
async fn parent_under(
    store: Rc<OrmStore>,
    limit: usize,
) -> (WorkflowService, AppWorkflows, Deployments, String) {
    let (service, app_id, _, deployments) = registered_service(store).await;
    service
        .fixture_register(
            &app_id,
            leased_policy(
                2,
                AppPolicy {
                    max_child_output_bytes: limit,
                    ..AppPolicy::default()
                },
            ),
        )
        .await
        .unwrap();
    let scope = service.fixture_app(app_id);
    let parent = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    (service, scope, deployments, parent.id)
}

fn child(ordinal: i32, name: &str) -> serde_json::Value {
    json!({"kind":"Child", "ordinal":ordinal, "name":name, "childWorkflowName":"Child"})
}

/// Take the next task, which must be `parent`'s, and settle it with `outcomes`.
async fn dispatch_parent(
    service: &WorkflowService,
    worker: &WorkerIdentity,
    parent: &str,
    outcomes: serde_json::Value,
) -> Vec<crate::engine::JournalStep> {
    let task = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, parent);
    let journal = task.invocation.journal.clone();
    service
        .complete(worker, &task.id, &task.token, execution(outcomes))
        .await
        .unwrap();
    journal
}

/// Complete every child task now runnable, each returning [`OUTPUT`].
async fn complete_children(
    service: &WorkflowService,
    scope: &AppWorkflows,
    worker: &WorkerIdentity,
    objects: &objects::Objects,
    count: usize,
) {
    for _ in 0..count {
        let task = service.poll(worker).await.unwrap().unwrap();
        let result = output_reference(OUTPUT);
        service
            .stage_payload(
                worker,
                &task.id,
                &task.token,
                &RequestId::mint(),
                result.clone(),
                objects.upload(OUTPUT),
            )
            .await
            .unwrap();
        service
            .complete(
                worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"RunCompleted", "outputRef":result}])),
            )
            .await
            .unwrap();
    }
    deliver_propagations(scope).await;
}

/// The parent's journal as its next dispatch replays it.
async fn replayed(
    service: &WorkflowService,
    worker: &WorkerIdentity,
    parent: &str,
) -> Vec<crate::engine::JournalStep> {
    let task = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, parent);
    task.invocation.journal
}

/// Assert `step` carries its child's output, or the limit failure instead.
fn assert_join(step: &crate::engine::JournalStep, attached: bool) {
    if attached {
        assert_eq!(step.state, "completed", "{step:?}");
        assert_eq!(step.output_ref, Some(output_reference(OUTPUT)), "{step:?}");
        assert!(step.error.is_none(), "{step:?}");
    } else {
        assert_eq!(step.state, "failed", "{step:?}");
        assert!(step.output_ref.is_none(), "{step:?}");
        let error = step
            .error
            .as_ref()
            .expect("a refused join carries its failure");
        assert_eq!(error["type"], json!("LimitExceededError"), "{error}");
        assert_eq!(error["retryable"], json!(false), "{error}");
    }
}

/// Both children complete before their parent is next examined, so one pass
/// attaches them. The lower ordinal is attached first, and the one that would
/// carry the sum past the bound is refused.
async fn together(store: Rc<OrmStore>, limit: usize) {
    let (service, scope, _deployments, parent) = parent_under(store, limit).await;
    let objects = objects::Objects::new();
    let worker = WorkerIdentity::new("children-together".into()).unwrap();
    dispatch_parent(
        &service,
        &worker,
        &parent,
        json!([child(0, "a"), child(1, "b")]),
    )
    .await;
    complete_children(&service, &scope, &worker, &objects, 2).await;
    let journal = replayed(&service, &worker, &parent).await;
    assert_join(&journal[0], true);
    assert_join(&journal[1], limit >= BOTH);
}

/// The second child completes after the first is already attached, so the sum
/// it is held to starts from what the journal carries.
async fn one_after_another(store: Rc<OrmStore>, limit: usize) {
    let (service, scope, _deployments, parent) = parent_under(store, limit).await;
    let objects = objects::Objects::new();
    let worker = WorkerIdentity::new("children-in-turn".into()).unwrap();
    dispatch_parent(&service, &worker, &parent, json!([child(0, "a")])).await;
    complete_children(&service, &scope, &worker, &objects, 1).await;
    let journal = dispatch_parent(&service, &worker, &parent, json!([child(1, "b")])).await;
    assert_join(&journal[0], true);
    complete_children(&service, &scope, &worker, &objects, 1).await;
    let journal = replayed(&service, &worker, &parent).await;
    assert_join(&journal[0], true);
    assert_join(&journal[1], limit >= BOTH);
}

/// A successor seeded by `continueAsNew` is a new run generation, so the child
/// outputs its predecessor attached do not count against it. Under [`OVER`] the
/// successor's first child fits on its own; if the predecessor's output carried
/// over, that child would be refused.
async fn continued(store: Rc<OrmStore>, limit: usize) {
    let (service, scope, _deployments, parent) = Box::pin(parent_under(store, limit)).await;
    let objects = objects::Objects::new();
    let worker = WorkerIdentity::new("children-continued".into()).unwrap();
    dispatch_parent(&service, &worker, &parent, json!([child(0, "a")])).await;
    complete_children(&service, &scope, &worker, &objects, 1).await;
    let journal =
        dispatch_parent(&service, &worker, &parent, json!([{"kind":"ContinueAsNew"}])).await;
    assert_join(&journal[0], true);
    let successor = scope
        .status(&parent)
        .await
        .unwrap()
        .continued_as_new_run_id
        .expect("a continuation mints a successor");
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, successor);
    assert!(
        task.invocation.journal.is_empty(),
        "the successor starts from nothing: {:?}",
        task.invocation.journal
    );
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([child(0, "b")])),
        )
        .await
        .unwrap();
    complete_children(&service, &scope, &worker, &objects, 1).await;
    let journal = replayed(&service, &worker, &successor).await;
    assert_join(&journal[0], true);
}

/// A restart counts the children its retained prefix keeps. The prefix holds one
/// child of [`OUTPUT`] bytes; under [`OVER`] the re-executed child that follows
/// would carry the sum past the bound, so it is refused. A restart that dropped
/// the prefix from its total would attach it.
async fn restarted(store: Rc<OrmStore>, limit: usize) {
    use crate::operations::{RestartOptions, RestartTarget};
    let (service, scope, _deployments, parent) = Box::pin(parent_under(store, limit)).await;
    let objects = objects::Objects::new();
    let worker = WorkerIdentity::new("children-restarted".into()).unwrap();
    dispatch_parent(&service, &worker, &parent, json!([child(0, "a")])).await;
    complete_children(&service, &scope, &worker, &objects, 1).await;
    // A step after the child, so the restart has a boundary that retains it.
    dispatch_parent(
        &service,
        &worker,
        &parent,
        json!([{"kind":"StepCompleted","ordinal":1,"name":"after","output":0}]),
    )
    .await;
    service
        .fixture_app(scope.app_id().clone())
        .restart(
            &RequestId::mint(),
            &parent,
            RestartOptions {
                from: Some(RestartTarget {
                    name: "after".into(),
                    occurrence: None,
                }),
                deploy: None,
            },
        )
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, parent);
    assert_eq!(task.generation, 1);
    assert_eq!(
        task.invocation.journal.len(),
        1,
        "the prefix holds the one child: {:?}",
        task.invocation.journal
    );
    assert_join(&task.invocation.journal[0], true);
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([child(1, "b")])),
        )
        .await
        .unwrap();
    complete_children(&service, &scope, &worker, &objects, 1).await;
    let journal = replayed(&service, &worker, &parent).await;
    assert_join(&journal[0], true);
    assert_join(&journal[1], limit >= BOTH);
}
