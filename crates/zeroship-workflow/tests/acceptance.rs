use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use serde_json::json;
use zeroship_workflow::operations::{
    ConflictPolicy, RestartOptions, RunOperation, RunState, SignalOptions, StartOptions,
};
use zeroship_workflow::{
    DevWorkflowEngine, WorkflowBackend, WorkflowExecution, WorkflowExecutor, WorkflowInvocation,
    WorkflowServiceError,
};

struct MustNotExecute;

#[async_trait(?Send)]
impl WorkflowExecutor for MustNotExecute {
    async fn dispatch(
        &self,
        _: &WorkflowInvocation,
    ) -> Result<WorkflowExecution, WorkflowServiceError> {
        panic!("an app operation must not wait for workflow execution")
    }
}

fn keyed(policy: ConflictPolicy) -> StartOptions {
    StartOptions {
        key: Some("checkout".into()),
        on_conflict: policy,
        ..Default::default()
    }
}

#[compio::test]
async fn local_mutations_return_after_commit_without_executing_app_code() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflows.sqlite");
    let engine = DevWorkflowEngine::open(&path, Arc::new(MustNotExecute)).unwrap();
    let app = engine.backend_for_app("app-a");
    let run = app
        .start("Checkout".into(), StartOptions::default())
        .await
        .unwrap();
    assert_eq!(run.state, RunState::Queued);
    app.signal(
        run.id.clone(),
        SignalOptions {
            signal_type: "approved".into(),
            payload: json!(true),
        },
    )
    .await
    .unwrap();
    app.restart(run.id.clone(), RestartOptions::default())
        .await
        .unwrap();

    // A separately opened connection observes accepted mutations immediately.
    let conn = Connection::open(path).unwrap();
    let persisted: (String, i64) = conn.query_row(
        "SELECT state, (SELECT COUNT(*) FROM workflow_signals WHERE run_id = workflow_runs.id) \
         FROM workflow_runs WHERE id = ?1", params![run.id], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(persisted, ("queued".into(), 1));
}

#[compio::test]
async fn local_conflict_policies_are_scoped_and_only_live_runs_own_start_keys() {
    let dir = tempfile::tempdir().unwrap();
    let engine = DevWorkflowEngine::open(
        dir.path().join("workflows.sqlite"),
        Arc::new(MustNotExecute),
    )
    .unwrap();
    let a = engine.backend_for_app("app-a");
    let b = engine.backend_for_app("app-b");
    let first = a
        .start("Checkout".into(), keyed(ConflictPolicy::Join))
        .await
        .unwrap();
    let other = b
        .start("Checkout".into(), keyed(ConflictPolicy::Reject))
        .await
        .unwrap();
    assert_ne!(first.id, other.id);
    a.transition(first.id.clone(), RunOperation::Pause)
        .await
        .unwrap();
    let joined = a
        .start("Checkout".into(), keyed(ConflictPolicy::Join))
        .await
        .unwrap();
    assert_eq!(joined.id, first.id);
    assert_eq!(joined.state, RunState::Paused);
    assert!(matches!(
        a.start("Checkout".into(), keyed(ConflictPolicy::Reject))
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let replacement = a
        .start("Checkout".into(), keyed(ConflictPolicy::Replace))
        .await
        .unwrap();
    assert_ne!(replacement.id, first.id);
    assert_eq!(
        a.status(first.id.clone()).await.unwrap().state,
        RunState::Cancelled
    );
    assert_eq!(
        b.status(other.id.clone()).await.unwrap().state,
        RunState::Queued
    );
    a.transition(replacement.id.clone(), RunOperation::Cancel)
        .await
        .unwrap();
    let reused = a
        .start("Checkout".into(), keyed(ConflictPolicy::Reject))
        .await
        .unwrap();
    assert_ne!(reused.id, replacement.id);
    let conn = Connection::open(engine.path()).unwrap();
    let deploy_apps: Vec<String> = conn
        .prepare("SELECT app_id FROM app_deploys ORDER BY app_id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(deploy_apps, ["app-a", "app-b"]);
}

#[test]
fn concurrent_connections_join_the_same_durable_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflows.sqlite");
    let first = DevWorkflowEngine::open(&path, Arc::new(MustNotExecute)).unwrap();
    let second = DevWorkflowEngine::open(&path, Arc::new(MustNotExecute)).unwrap();
    let barrier = std::sync::Barrier::new(2);
    let ids = std::thread::scope(|scope| {
        let tasks: Vec<_> = [first, second]
            .into_iter()
            .map(|engine| {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    compio::runtime::Runtime::new()
                        .unwrap()
                        .block_on(async move {
                            engine
                                .backend_for_app("app")
                                .start("Checkout".into(), keyed(ConflictPolicy::Join))
                                .await
                                .unwrap()
                                .id
                        })
                })
            })
            .collect();
        tasks
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(ids[0], ids[1]);
    let count: i64 = Connection::open(path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM workflow_runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[compio::test]
async fn local_signal_and_wake_up_rollback_together() {
    let dir = tempfile::tempdir().unwrap();
    let engine = DevWorkflowEngine::open(
        dir.path().join("workflows.sqlite"),
        Arc::new(MustNotExecute),
    )
    .unwrap();
    let app = engine.backend_for_app("app");
    let run = app
        .start("Checkout".into(), StartOptions::default())
        .await
        .unwrap();
    let conn = Connection::open(engine.path()).unwrap();
    conn.execute("UPDATE workflow_runs SET state = 'waiting', waiting_step_key = 'wait:0:approval:approved', wake_at = NULL WHERE id = ?1", params![run.id]).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_wake BEFORE UPDATE OF wake_at ON workflow_runs BEGIN SELECT RAISE(ABORT, 'injected wake failure'); END;").unwrap();
    assert!(matches!(
        app.signal(
            run.id,
            SignalOptions {
                signal_type: "approved".into(),
                payload: json!(true)
            }
        )
        .await,
        Err(WorkflowServiceError::Internal(_))
    ));
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM workflow_signals", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[compio::test]
async fn native_callers_cannot_bypass_validation_or_another_apps_scope() {
    let dir = tempfile::tempdir().unwrap();
    let engine = DevWorkflowEngine::open(
        dir.path().join("workflows.sqlite"),
        Arc::new(MustNotExecute),
    )
    .unwrap();
    let a = engine.backend_for_app("app-a");
    assert!(matches!(
        a.start("__zs.private".into(), StartOptions::default())
            .await,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    assert!(matches!(
        a.start(
            "Checkout".into(),
            StartOptions {
                key: Some(String::new()),
                ..Default::default()
            }
        )
        .await,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    let run = a
        .start("Checkout".into(), StartOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        a.signal(
            run.id.clone(),
            SignalOptions {
                signal_type: "__zs.child:0".into(),
                payload: json!(true)
            }
        )
        .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let b = engine.backend_for_app("app-b");
    assert!(matches!(
        b.status(run.id.clone()).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        b.transition(run.id.clone(), RunOperation::Cancel).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        b.signal(
            run.id.clone(),
            SignalOptions {
                signal_type: "approved".into(),
                payload: json!(true)
            }
        )
        .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert_eq!(a.status(run.id).await.unwrap().state, RunState::Queued);
}
