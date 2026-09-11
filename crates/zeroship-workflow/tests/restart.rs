use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use serde_json::json;
use zeroship_workflow::engine::StepOutcome;
use zeroship_workflow::operations::{RestartDeploy, RestartOptions, RestartTarget, StartOptions};
use zeroship_workflow::{
    DevWorkflowEngine, WorkflowBackend, WorkflowExecutor, WorkflowServiceError,
};
use zeroship_workflow::{WorkflowExecution, WorkflowInvocation};

struct Complete;

#[async_trait(?Send)]
impl WorkflowExecutor for Complete {
    async fn dispatch(
        &self,
        _: &WorkflowInvocation,
    ) -> Result<WorkflowExecution, WorkflowServiceError> {
        Ok(WorkflowExecution {
            outcomes: vec![StepOutcome::RunCompleted {
                output: Some(json!("original")),
                output_ref: None,
            }],
        })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    engine: Arc<DevWorkflowEngine>,
    conn: Connection,
    app: String,
    run: String,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflows.sqlite");
        let engine = DevWorkflowEngine::open(&path, Arc::new(Complete)).unwrap();
        let app = uuid::Uuid::now_v7().to_string();
        let run = engine
            .backend_for_app(&app)
            .start("Checkout".into(), StartOptions::default())
            .await
            .unwrap()
            .id;
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO workflow_steps (run_id, ordinal, name, kind, state, output, batch_id, started_at, consumed_signal_id) \
             VALUES (?1, 0, 'keep', 'wait_signal', 'completed', '1', 'seed', 0, 'signal_keep'), \
                    (?1, 1, 'repeat', 'wait_signal', 'completed', '2', 'seed', 0, 'signal_repeat')",
            params![run],
        ).unwrap();
        conn.execute(
            "INSERT INTO workflow_signals (id, run_id, type, payload, created_at, consumed_by) \
             VALUES ('signal_keep', ?1, 'approved', '{}', 0, ?1), ('signal_repeat', ?1, 'approved', '{}', 0, ?1)",
            params![run],
        ).unwrap();
        Self {
            _dir: dir,
            engine,
            conn,
            app,
            run,
        }
    }

    fn history(&self) -> Vec<(String, Option<String>)> {
        self.conn
            .prepare(
                "SELECT step.name, signal.consumed_by FROM workflow_steps step \
             JOIN workflow_signals signal ON signal.id = step.consumed_signal_id \
             WHERE step.run_id = ?1 ORDER BY step.ordinal",
            )
            .unwrap()
            .query_map(params![self.run], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn partial() -> RestartOptions {
        RestartOptions {
            from: Some(RestartTarget {
                name: "repeat".into(),
                occurrence: None,
            }),
            deploy: None,
        }
    }
}

#[compio::test]
async fn partial_restart_retains_the_prefix_and_pin_and_rewinds_only_discarded_signals() {
    let fx = Fixture::new().await;
    let started_pin: String = fx
        .conn
        .query_row(
            "SELECT deploy_id FROM workflow_runs WHERE id = ?1",
            params![fx.run],
            |row| row.get(0),
        )
        .unwrap();
    let latest =
        zeroship_core::typed_id::from_uuid_string("dep", &uuid::Uuid::now_v7().to_string())
            .unwrap();
    fx.conn.execute(
        "INSERT INTO app_deploys (id, app_id, deploy_hash, manifest_json, created_at, activated_at) \
         VALUES (?1, ?2, 'latest', '{}', 0, 9223372036854775807)", params![latest, fx.app],
    ).unwrap();

    let backend = fx.engine.backend_for_app(&fx.app);
    let result = backend
        .restart(fx.run.clone(), Fixture::partial())
        .await
        .unwrap();
    assert_eq!(result.run_id, fx.run);
    assert_eq!(result.pinned_to, started_pin);
    assert_eq!(result.restarted_from_ordinal, Some(1));
    assert_eq!(fx.history(), vec![("keep".into(), Some(fx.run.clone()))]);
    let consumed: Option<String> = fx
        .conn
        .query_row(
            "SELECT consumed_by FROM workflow_signals WHERE id = 'signal_repeat'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(consumed, None);

    let result = backend
        .restart(fx.run.clone(), RestartOptions::default())
        .await
        .unwrap();
    assert_eq!(result.pinned_to, latest);
    assert_eq!(result.restarted_from_ordinal, None);
    assert!(fx.history().is_empty());
}

#[compio::test]
async fn restart_rejects_scope_override_live_execution_and_active_compensation_without_deleting_history(
) {
    let fx = Fixture::new().await;
    let original = fx.history();
    let other = fx.engine.backend_for_app(&uuid::Uuid::now_v7().to_string());
    assert!(matches!(
        other
            .restart(fx.run.clone(), RestartOptions::default())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let backend = fx.engine.backend_for_app(&fx.app);
    for (state, lease, paused_from) in [
        ("running", Some(i64::MAX), None),
        ("paused", Some(i64::MAX), Some("running")),
        ("compensating", None, None),
        ("paused", None, Some("compensating")),
    ] {
        fx.conn.execute(
            "UPDATE workflow_runs SET state = ?2, lease_expires = ?3, paused_from_status = ?4 WHERE id = ?1",
            params![fx.run, state, lease, paused_from],
        ).unwrap();
        assert!(
            matches!(
                backend
                    .restart(fx.run.clone(), RestartOptions::default())
                    .await,
                Err(WorkflowServiceError::Conflict(_))
            ),
            "state={state}"
        );
        assert_eq!(fx.history(), original);
    }
}

#[compio::test]
async fn restart_guards_descendants_compensated_prefix_and_repinning() {
    let fx = Fixture::new().await;
    let backend = fx.engine.backend_for_app(&fx.app);
    let child = zeroship_core::typed_id::new_workflow_run_id();
    fx.conn.execute(
        "INSERT INTO workflow_runs (id, workflow_name, app_id, deploy_id, state, parent_run_id, started_at, created_at) \
         SELECT ?2, workflow_name, app_id, deploy_id, 'waiting', id, 0, 0 FROM workflow_runs WHERE id = ?1",
        params![fx.run, child],
    ).unwrap();
    assert!(matches!(
        backend
            .restart(fx.run.clone(), RestartOptions::default())
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    fx.conn
        .execute(
            "UPDATE workflow_runs SET state = 'completed' WHERE id = ?1",
            params![child],
        )
        .unwrap();
    fx.conn.execute("UPDATE workflow_steps SET compensation_finished_at = 0 WHERE run_id = ?1 AND ordinal = 0",
        params![fx.run]).unwrap();
    assert!(matches!(
        backend.restart(fx.run.clone(), Fixture::partial()).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let mut repin = Fixture::partial();
    repin.deploy = Some(RestartDeploy::Latest);
    assert!(matches!(
        backend.restart(fx.run.clone(), repin).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    backend
        .restart(fx.run.clone(), RestartOptions::default())
        .await
        .unwrap();
    assert!(fx.history().is_empty());
}

#[compio::test]
async fn failed_restart_rolls_back_checkpoint_and_signal_rewrites() {
    let fx = Fixture::new().await;
    let original = fx.history();
    fx.conn
        .execute_batch(
            "CREATE TRIGGER reject_restart BEFORE UPDATE OF restart_count ON workflow_runs \
         BEGIN SELECT RAISE(ABORT, 'injected restart failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        fx.engine
            .backend_for_app(&fx.app)
            .restart(fx.run.clone(), Fixture::partial())
            .await,
        Err(WorkflowServiceError::Internal(_))
    ));
    assert_eq!(fx.history(), original);
}
