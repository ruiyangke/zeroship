use super::*;
use std::{cell::Cell, rc::Rc, time::Duration};
use zeroship_workflow::service::{
    runner::{RunnerOutcome, RunnerSlot, TaskExecution, TaskExecutor},
    RemoteTasks, TaskAssignment,
};

struct Executor(Rc<Cell<bool>>);
impl TaskExecutor for Executor {
    fn start(
        &self,
        assignment: &TaskAssignment,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        Ok(Box::new(Execution {
            stopped: self.0.clone(),
            run: assignment.invocation.run_id.clone(),
        }))
    }
}
struct Execution {
    stopped: Rc<Cell<bool>>,
    run: String,
}
#[async_trait::async_trait(?Send)]
impl TaskExecution for Execution {
    async fn wait(&mut self) -> Result<WorkflowExecution, WorkflowServiceError> {
        WorkflowExecution::from_runtime_value(json!({"outcomes":[{
            "kind":"RunCompleted", "output":{"run":self.run},
        }]}))
    }
    fn cancel(&mut self) {}
    async fn stop(&mut self) {
        self.stopped.set(true);
    }
}

pub(super) async fn check(
    tasks: RemoteTasks,
    app: &zeroship_workflow::service::RemoteAppWorkflows,
) {
    let run = app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let stopped = Rc::new(Cell::new(false));
    let mut slot = RunnerSlot::new(
        Rc::new(tasks),
        Rc::new(Executor(stopped.clone())),
        Duration::from_secs(5),
    )
    .unwrap();
    let RunnerOutcome::Completed(receipt) = slot.run_once().await.unwrap() else {
        panic!("the remote runner must discover and complete the accepted run");
    };
    assert_eq!(receipt.run_id, run.id);
    assert!(stopped.get());
    assert_eq!(
        app.status(&run.id).await.unwrap().output,
        Some(json!({"run":run.id}))
    );
    assert!(matches!(
        slot.run_once().await.unwrap(),
        RunnerOutcome::Idle
    ));
}
