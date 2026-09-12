use super::*;
use crate::{
    engine::{StepOutcome, WorkflowOutputRef},
    operations::RunState,
    service::{
        runner::{
            PreparedExecution, TaskPayloadLimits, TaskPayloadReader, TaskPayloads, TaskTransport,
            WorkerTasks,
        },
        AppWorkflows, PayloadRead, PayloadSlot, StagedPayload, TaskAssignment, TaskToken,
        WorkerIdentity,
    },
};
use async_trait::async_trait;
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use zeroship_storage::{backend::BoxChunkSource, LocalFs, StorageStore};

const LIMITS: TaskPayloadLimits = TaskPayloadLimits {
    max_inline_bytes: 32,
    max_payload_bytes: 256,
    max_result_bytes: 4096,
};

struct LostReceipt {
    inner: WorkerTasks,
    lose: Cell<bool>,
    requests: RefCell<Vec<String>>,
}
#[async_trait(?Send)]
impl TaskPayloads for LostReceipt {
    async fn executable(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<zeroship_bundle::LoadedWorker, WorkflowServiceError> {
        TaskPayloads::executable(&self.inner, task, token).await
    }
    async fn stage(
        &self,
        task: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: BoxChunkSource,
    ) -> Result<StagedPayload, WorkflowServiceError> {
        self.requests.borrow_mut().push(request.as_str().to_owned());
        let receipt = self
            .inner
            .stage(task, token, request, reference, body)
            .await?;
        if self.lose.replace(false) {
            Err(WorkflowServiceError::Unavailable(
                "lost upload receipt".into(),
            ))
        } else {
            Ok(receipt)
        }
    }
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.inner.read(task, token, reference).await
    }
}

async fn claim(app: &AppWorkflows, tasks: &WorkerTasks) -> TaskAssignment {
    let run = app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = tasks.poll().await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run.id);
    task
}
fn prepare(task: &TaskAssignment, outcomes: Value) -> PreparedExecution {
    PreparedExecution::from_runtime_json(task, &json!({"outcomes":outcomes}).to_string(), LIMITS)
        .unwrap()
}
fn step(value: Value) -> Value {
    json!({"kind":"StepCompleted","ordinal":0,"name":"saved","output":value})
}
async fn stored_rows(store: &OrmStore, task: &TaskAssignment) -> usize {
    let tx = store.begin().await.unwrap();
    let rows = journal_rows(&tx, "payloads", json!({"task_id":task.id})).await;
    tx.commit().await.unwrap();
    rows.len()
}

#[compio::test]
async fn sqlite_output_preparation_and_retryable_uploads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    output_contract(Rc::new(sqlite_store(&path).await)).await;
}
#[compio::test]
async fn postgres_output_preparation_and_retryable_uploads() {
    let fixture = PostgresFixture::start().await;
    output_contract(Rc::new(fixture.store.clone())).await;
}

async fn output_contract(store: Rc<OrmStore>) {
    let dir = tempfile::tempdir().unwrap();
    let (service, app, other, _deployments) = registered_service(store.clone()).await;
    let service = service
        .with_payload_storage(StorageStore::from_backend(Arc::new(LocalFs::new(
            dir.path(),
        ))))
        .unwrap();
    let scope = service.for_app(app.clone());
    let tasks = service.tasks(WorkerIdentity::new("output-writer".into()).unwrap());

    // An inline value needs no payload object, including JSON null.
    for value in [json!({"small":true}), Value::Null] {
        let task = claim(&scope, &tasks).await;
        let prepared = prepare(
            &task,
            json!([step(value.clone()), {"kind":"RunCompleted","output":value}]),
        );
        let execution = prepared.stage(&tasks).await.unwrap();
        assert_eq!(stored_rows(store.as_ref(), &task).await, 0);
        tasks
            .complete(&task.id, &task.token, execution)
            .await
            .unwrap();
        let bytes = scope
            .read_step_output(&task.invocation.run_id, "saved", 0)
            .await
            .unwrap()
            .into_bytes(256)
            .await
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), value);
    }

    // Small explicit references and automatic large results use the same path.
    for mode in ["auto", "ref", "blob", "stream"] {
        let task = claim(&scope, &tasks).await;
        let value = if mode == "auto" {
            json!("x".repeat(64))
        } else {
            json!({"small":mode})
        };
        let mut saved = step(value.clone());
        saved["outputMode"] = json!(mode);
        saved["outputContentType"] = json!("application/vnd.workflow+json");
        let prepared = prepare(&task, json!([saved]));
        let execution = prepared.stage(&tasks).await.unwrap();
        let StepOutcome::StepCompleted {
            output, output_ref, ..
        } = &execution.outcomes[0]
        else {
            panic!("completed step required")
        };
        assert!(output.is_none());
        assert_eq!(
            output_ref.as_ref().unwrap().content_type.as_deref(),
            Some("application/vnd.workflow+json")
        );
        tasks
            .complete(&task.id, &task.token, execution)
            .await
            .unwrap();
        let replay = tasks.poll().await.unwrap().unwrap();
        let reader = TaskPayloadReader::new(Rc::new(tasks.clone()), &replay, 256).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&reader.read_step_output("saved", 0).await.unwrap())
                .unwrap(),
            value
        );
        tasks
            .complete(
                &replay.id,
                &replay.token,
                super::execution(json!([{"kind":"RunCompleted"}])),
            )
            .await
            .unwrap();
    }

    // A lost receipt retries the durable upload identity and deduplicates a
    // shared step/root value before the journal admits either reference.
    let task = claim(&scope, &tasks).await;
    let value = json!({"large":"x".repeat(64)});
    let prepared = prepare(
        &task,
        json!([step(value.clone()), {"kind":"RunCompleted","output":value}]),
    );
    let fault = LostReceipt {
        inner: tasks.clone(),
        lose: Cell::new(true),
        requests: RefCell::default(),
    };
    assert!(matches!(
        prepared.stage(&fault).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    let execution = prepared.stage(&fault).await.unwrap();
    assert_eq!(fault.requests.borrow().len(), 2);
    assert_eq!(fault.requests.borrow()[0], fault.requests.borrow()[1]);
    assert_eq!(stored_rows(store.as_ref(), &task).await, 1);
    let receipt = tasks
        .complete(&task.id, &task.token, execution)
        .await
        .unwrap();
    assert_eq!(receipt.state, RunState::Completed);
    let bytes = scope
        .read_payload(&receipt.run_id, 0, PayloadSlot::Output)
        .await
        .unwrap()
        .into_bytes(256)
        .await
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), value);
    assert!(matches!(
        service
            .for_app(other)
            .read_payload(&receipt.run_id, 0, PayloadSlot::Output)
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));

    let task = claim(&scope, &tasks).await;
    let execution = prepare(&task, json!([{"kind":"ContinueAsNew","input":value}]))
        .stage(&tasks)
        .await
        .unwrap();
    let continued = tasks
        .complete(&task.id, &task.token, execution)
        .await
        .unwrap();
    let replay = tasks.poll().await.unwrap().unwrap();
    assert_eq!(continued.run_id, task.invocation.run_id);
    assert_ne!(replay.invocation.run_id, continued.run_id);
    let reader = TaskPayloadReader::new(Rc::new(tasks.clone()), &replay, 256).unwrap();
    assert_eq!(reader.input().await.unwrap(), Some(value));
    tasks
        .complete(
            &replay.id,
            &replay.token,
            super::execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();

    // Host or service limits retain the accepted prefix and stop the frontier.
    for (revision, mode, length, service_limit) in [
        (2, "inline", 64, 256),
        (3, "auto", 300, 256),
        (4, "auto", 64, 16),
    ] {
        service
            .register_app(
                &app,
                super::configured_policy(
                    revision,
                    AppPolicy {
                        max_payload_bytes: service_limit,
                        ..AppPolicy::default()
                    },
                ),
            )
            .await
            .unwrap();
        let task = claim(&scope, &tasks).await;
        let execution = prepare(&task, json!([
            step(json!("prefix")),
            {"kind":"StepCompleted","ordinal":1,"name":"large","output":"x".repeat(length),"outputMode":mode},
            {"kind":"RunCompleted","output":"must not commit"}
        ])).stage(&tasks).await.unwrap();
        assert_eq!(execution.outcomes.len(), 2);
        assert!(
            matches!(&execution.outcomes[1], StepOutcome::RunFailed{error,..} if error["type"] == "LimitExceededError" && error["retryable"] == false)
        );
        tasks
            .complete(&task.id, &task.token, execution)
            .await
            .unwrap();
        assert_eq!(
            scope.status(&task.invocation.run_id).await.unwrap().state,
            RunState::Failed
        );
        assert_eq!(
            scope
                .read_step_output(&task.invocation.run_id, "saved", 0)
                .await
                .unwrap()
                .into_bytes(256)
                .await
                .unwrap(),
            br#""prefix""#
        );
        assert_eq!(stored_rows(store.as_ref(), &task).await, 0);
    }

    let task = claim(&scope, &tasks).await;
    for outcomes in [
        json!([step(json!("x".repeat(64))), {"kind":"RunCompleted","outputMode":"blob"}]),
        json!([{ "kind":"StepCompleted","ordinal":0,"name":"invalid","outputMode":"unknown" }]),
        json!([{ "kind":"RunCompleted","output":true,"outputRef":{"hash":"a".repeat(64),"size":1} }]),
    ] {
        assert!(PreparedExecution::from_runtime_json(
            &task,
            &json!({"outcomes":outcomes}).to_string(),
            LIMITS
        )
        .is_err());
    }
    assert_eq!(stored_rows(store.as_ref(), &task).await, 0);
    let oversized = PreparedExecution::from_runtime_json(
        &task,
        &"x".repeat(LIMITS.max_result_bytes + 1),
        LIMITS,
    )
    .unwrap();
    let result = oversized.stage(&tasks).await.unwrap();
    assert!(matches!(
        &result.outcomes[..],
        [StepOutcome::RunFailed { .. }]
    ));
    tasks.complete(&task.id, &task.token, result).await.unwrap();
}

#[test]
fn output_budgets_reject_unusable_limits() {
    TaskPayloadLimits::default().validate().unwrap();
    for limits in [
        TaskPayloadLimits {
            max_inline_bytes: 0,
            ..LIMITS
        },
        TaskPayloadLimits {
            max_inline_bytes: LIMITS.max_payload_bytes + 1,
            ..LIMITS
        },
        TaskPayloadLimits {
            max_result_bytes: LIMITS.max_payload_bytes - 1,
            ..LIMITS
        },
    ] {
        assert!(limits.validate().is_err());
    }
}
