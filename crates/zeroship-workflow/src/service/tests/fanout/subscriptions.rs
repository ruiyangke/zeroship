//! A run subscribed to one topic more than once in one generation.

use super::*;
use crate::operations::RunOperation;

/// Start a run that waits on the `updates` topic from two steps of one
/// generation. Its first execution starts a child and suspends on one topic
/// wait; the child's completion replays it while that wait is still pending,
/// and the replay suspends on a second wait on the same topic.
async fn wait_twice(
    service: &WorkflowService,
    scope: &AppWorkflows,
    worker: &WorkerIdentity,
) -> String {
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run.id);
    service
        .complete(
            worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"Child","ordinal":0,"name":"join","childWorkflowName":"Child","options":{}},
                {"kind":"Wait","ordinal":1,"name":"first","signalType":"news","topic":"updates"},
            ])),
        )
        .await
        .unwrap();
    let child = service.poll(worker).await.unwrap().unwrap();
    assert_ne!(child.invocation.run_id, run.id);
    service
        .complete(
            worker,
            &child.id,
            &child.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    assert!(!deliver_propagations(scope).await.is_empty());
    let replay = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(replay.invocation.run_id, run.id);
    assert_eq!(replay.invocation.journal[1].state, "running");
    service
        .complete(
            worker,
            &replay.id,
            &replay.token,
            execution(json!([{"kind":"Wait","ordinal":2,"name":"second","signalType":"news","topic":"updates"}])),
        )
        .await
        .unwrap();
    let tx = service.begin().await.unwrap();
    let subscriptions = journal_rows(
        &tx,
        "subscriptions",
        json!({"app_id":scope.app_id().as_str(), "run_id":run.id}),
    )
    .await;
    tx.commit().await.unwrap();
    let held: Vec<_> = subscriptions
        .iter()
        .map(|row| {
            (
                row.integer("generation").unwrap(),
                row.integer("ordinal").unwrap(),
                row.text("topic").unwrap(),
            )
        })
        .collect();
    assert_eq!(
        held,
        [(0, 1, "updates".to_owned()), (0, 2, "updates".to_owned())],
        "the run holds two subscriptions to the topic in one generation"
    );
    run.id
}

/// The broadcast signals `run` holds, with the wait each is targeted at.
async fn signals_of(scope: &AppWorkflows, run: &str) -> Vec<Option<i64>> {
    let tx = scope.service.begin().await.unwrap();
    let rows = journal_rows(
        &tx,
        "signals",
        json!({"app_id":scope.app_id().as_str(), "run_id":run, "delivery":"topic"}),
    )
    .await;
    tx.commit().await.unwrap();
    rows.iter()
        .map(|row| row.optional_integer("target_ordinal").unwrap())
        .collect()
}

/// The run's frontier revision and due time.
async fn frontier(scope: &AppWorkflows, run: &str) -> (i64, Option<i64>) {
    let tx = scope.service.begin().await.unwrap();
    let row = journal_rows(
        &tx,
        "runs",
        json!({"app_id":scope.app_id().as_str(), "id":run}),
    )
    .await
    .remove(0);
    tx.commit().await.unwrap();
    (
        row.integer("frontier_revision").unwrap(),
        row.optional_integer("due_at").unwrap(),
    )
}

/// One page delivers both subscriptions of an idle run: the page commits, the
/// run receives the broadcast once, targeted at its first subscription's wait,
/// and is woken once, beside another recipient.
pub(super) async fn idle(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = Box::pin(registered_service(store)).await;
    let scope = service.fixture_app(app.clone());
    let worker = WorkerIdentity::new("fanout-twice".into()).unwrap();
    let twice = wait_twice(&service, &scope, &worker).await;
    let other = wait_on_topic(&service, &scope, &worker).await;
    let (revision, due) = frontier(&scope, &twice).await;
    assert_eq!(due, None);
    let accepted = broadcast(&scope, "both").await;
    let page = job(&scope, &accepted.id, 1).await;
    assert!(scope
        .fanout_job(&Grant::new(&page), FanoutOptions::default())
        .await
        .expect("the page commits")
        .is_some());
    assert_eq!(signals_of(&scope, &twice).await, [Some(1)]);
    assert_eq!(signals_of(&scope, &other).await.len(), 1);
    let (woken, due) = frontier(&scope, &twice).await;
    assert_eq!(woken, revision + 1, "the run is woken once");
    assert!(due.is_some());
    // Each woken run is handed out once. The targeted wait reads the signal;
    // the run's later wait on the topic is still pending.
    let mut runs = std::collections::BTreeSet::new();
    while let Some(task) = service.poll(&worker).await.unwrap() {
        assert!(runs.insert(task.invocation.run_id.clone()));
        if task.invocation.run_id == twice {
            let first = &task.invocation.journal[1];
            assert_eq!(first.state, "completed");
            assert_eq!(first.output.as_ref().unwrap()["payload"], json!("both"));
            assert_eq!(task.invocation.journal[2].state, "running");
        }
    }
    assert_eq!(runs, [twice, other].into_iter().collect());
}

/// One page delivers both subscriptions of a run that may not be woken: the
/// page commits, the run receives the broadcast once and stays asleep, and the
/// page's other recipient is still delivered and woken.
pub(super) async fn not_idle(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = Box::pin(registered_service(store)).await;
    let scope = service.fixture_app(app.clone());
    let worker = WorkerIdentity::new("fanout-twice-held".into()).unwrap();
    let twice = wait_twice(&service, &scope, &worker).await;
    let other = wait_on_topic(&service, &scope, &worker).await;
    scope
        .transition(&RequestId::mint(), &twice, RunOperation::Pause)
        .await
        .unwrap();
    let held = frontier(&scope, &twice).await;
    let (other_revision, _) = frontier(&scope, &other).await;
    let accepted = broadcast(&scope, "held").await;
    let page = job(&scope, &accepted.id, 1).await;
    assert!(scope
        .fanout_job(&Grant::new(&page), FanoutOptions::default())
        .await
        .expect("the page commits")
        .is_some());
    assert_eq!(signals_of(&scope, &twice).await, [Some(1)]);
    assert_eq!(
        frontier(&scope, &twice).await,
        held,
        "the held run is not woken"
    );
    assert_eq!(signals_of(&scope, &other).await.len(), 1);
    let (revision, due) = frontier(&scope, &other).await;
    assert_eq!(revision, other_revision + 1);
    assert!(due.is_some());
}
