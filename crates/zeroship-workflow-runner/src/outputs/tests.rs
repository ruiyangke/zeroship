use super::*;
use crate::{
    journal_fixture::{
        journal_row_count, leased_policy, registered_service, sqlite_store, PostgresFixture,
    },
    service_binding::ServiceFixture,
    PayloadObjects, PayloadRead, RunPayloads, TaskPayloadReader, TaskPayloads, TaskTransport,
    WorkerBinding, WorkerTasks,
};
use serde_json::json;
use std::sync::Arc;
use zeroship_workflow::{
    engine::{StepOutcome, WorkflowOutputRef},
    operations::{RunState, StartOptions},
    service::{
        schema, store::OrmStore, AppPolicy, AppWorkflows, PayloadSlot, RequestId, StagedPayload,
        TaskAssignment, TaskToken, WorkerIdentity,
    },
    WorkflowServiceError,
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
    let rows = journal_row_count(&tx, "payloads", json!({"task_id":task.id})).await;
    tx.commit().await.unwrap();
    rows
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

#[compio::test]
async fn sqlite_every_run_output_is_a_payload_reference() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    run_output_reference_contract(Rc::new(sqlite_store(&path).await)).await;
}
#[compio::test]
async fn postgres_every_run_output_is_a_payload_reference() {
    let fixture = PostgresFixture::start().await;
    run_output_reference_contract(Rc::new(fixture.store.clone())).await;
}

/// A run's result is a payload object whatever it weighs, and a run that
/// produced no result stages nothing.
///
/// Size decides nothing here: `{"ok":true}` sits far inside
/// `LIMITS.max_inline_bytes`, which the first arm asserts rather than assumes,
/// and it is staged anyway because the journal keeps no inline slot for a run
/// result. The empty-handed run is the control that separates "every result is
/// referenced" from "every close mints an object". The last arm holds the cost
/// that buys: the object a completing run stages is spent against
/// `max_payload_objects` like any other, so a tiny result is not free.
async fn run_output_reference_contract(store: Rc<OrmStore>) {
    let dir = tempfile::tempdir().unwrap();
    let (service, app, other, _deployments) = registered_service(store.clone()).await;
    let objects =
        PayloadObjects::open(StorageStore::from_backend(Arc::new(LocalFs::new(dir.path()))))
            .unwrap();
    let scope = service.fixture_app(app.clone());
    let tasks = service.tasks(
        WorkerIdentity::new("run-output-writer".into()).unwrap(),
        objects.clone(),
    );

    let value = json!({"ok":true});
    let encoded = serde_json::to_vec(&value).unwrap();
    assert!(
        encoded.len() < LIMITS.max_inline_bytes,
        "the forced value must be one the size rule would have kept inline"
    );
    let task = claim(&scope, &tasks).await;
    let execution = prepare(&task, json!([{"kind":"RunCompleted","output":value}]))
        .stage(&tasks)
        .await
        .unwrap();
    let StepOutcome::RunCompleted { output, output_ref } = &execution.outcomes[0] else {
        panic!("completed run required: {:?}", execution.outcomes[0])
    };
    assert!(output.is_none(), "a run result must not stay inline: {output:?}");
    let descriptor = output_ref.clone().expect("a run result must be referenced");
    assert_eq!(descriptor.size, i64::try_from(encoded.len()).unwrap());
    assert_eq!(descriptor.content_type.as_deref(), Some("application/json"));
    assert_eq!(stored_rows(store.as_ref(), &task).await, 1);
    let receipt = tasks
        .complete(&task.id, &task.token, execution)
        .await
        .unwrap();
    assert_eq!(receipt.state, RunState::Completed);

    // The descriptor `StatusOutputRef` declares in packages/workflows/src/index.ts.
    assert_eq!(
        scope.status(&receipt.run_id).await.unwrap().output,
        Some(json!({
            "kind":"ref",
            "ref":format!("wfblob:sha256:{}", descriptor.hash),
            "hash":descriptor.hash,
            "size":descriptor.size,
            "contentType":"application/json",
        })),
    );
    assert_eq!(
        serde_json::from_slice::<Value>(
            &scope
                .payloads(&objects)
                .read(&receipt.run_id, 0, PayloadSlot::Output)
                .await
                .unwrap()
                .into_bytes(256)
                .await
                .unwrap()
        )
        .unwrap(),
        value,
    );

    // The control: a run that produced no result references nothing and stages
    // nothing, so the arm above measures the result and not the close.
    let task = claim(&scope, &tasks).await;
    let execution = prepare(&task, json!([{"kind":"RunCompleted"}]))
        .stage(&tasks)
        .await
        .unwrap();
    assert!(
        matches!(
            &execution.outcomes[0],
            StepOutcome::RunCompleted {
                output: None,
                output_ref: None
            }
        ),
        "{:?}",
        execution.outcomes[0]
    );
    assert_eq!(stored_rows(store.as_ref(), &task).await, 0);
    let receipt = tasks
        .complete(&task.id, &task.token, execution)
        .await
        .unwrap();
    assert_eq!(receipt.state, RunState::Completed);
    assert_eq!(scope.status(&receipt.run_id).await.unwrap().output, None);

    // The budget. A second app starts with no objects, so one completing run
    // with a result spends its whole allowance.
    let budgeted = service.fixture_app(other.clone());
    service
        .fixture_register(
            &other,
            leased_policy(
                2,
                AppPolicy {
                    max_payload_objects: 1,
                    ..AppPolicy::default()
                },
            ),
        )
        .await
        .unwrap();
    let task = claim(&budgeted, &tasks).await;
    let spent = prepare(&task, json!([{"kind":"RunCompleted","output":value}]))
        .stage(&tasks)
        .await
        .unwrap();
    tasks
        .complete(&task.id, &task.token, spent)
        .await
        .unwrap();
    let task = claim(&budgeted, &tasks).await;
    assert!(
        matches!(
            prepare(&task, json!([{"kind":"RunCompleted","output":value}]))
                .stage(&tasks)
                .await,
            Err(WorkflowServiceError::ResourceExhausted(_))
        ),
        "a tiny run result must be charged against the object budget"
    );
    // The control for the budget: the same exhausted allowance still closes a
    // run that produced no result, so the refusal above is the object and not
    // the app being out of credit for everything.
    let execution = prepare(&task, json!([{"kind":"RunCompleted"}]))
        .stage(&tasks)
        .await
        .unwrap();
    assert_eq!(
        tasks
            .complete(&task.id, &task.token, execution)
            .await
            .unwrap()
            .state,
        RunState::Completed
    );
}

async fn output_contract(store: Rc<OrmStore>) {
    let dir = tempfile::tempdir().unwrap();
    let (service, app, other, _deployments) = registered_service(store.clone()).await;
    let objects =
        PayloadObjects::open(StorageStore::from_backend(Arc::new(LocalFs::new(dir.path()))))
            .unwrap();
    let scope = service.fixture_app(app.clone());
    let tasks = service.tasks(
        WorkerIdentity::new("output-writer".into()).unwrap(),
        objects.clone(),
    );

    // A step value inside the inline budget needs no payload object, including
    // JSON null. The run's own result is outside that claim - it is staged
    // whatever it weighs - so these batches close their run empty-handed and
    // `every_run_output_is_a_payload_reference` holds the other half.
    for value in [json!({"small":true}), Value::Null] {
        let task = claim(&scope, &tasks).await;
        let prepared = prepare(
            &task,
            json!([step(value.clone()), {"kind":"RunCompleted"}]),
        );
        let execution = prepared.stage(&tasks).await.unwrap();
        assert_eq!(stored_rows(store.as_ref(), &task).await, 0);
        tasks
            .complete(&task.id, &task.token, execution)
            .await
            .unwrap();
        let bytes = scope
            .payloads(&objects)
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
                crate::journal_fixture::execution(json!([{"kind":"RunCompleted"}])),
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
        .payloads(&objects)
        .read(&receipt.run_id, 0, PayloadSlot::Output)
        .await
        .unwrap()
        .into_bytes(256)
        .await
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), value);
    assert!(matches!(
        service
            .fixture_app(other)
            .payloads(&objects)
            .read(&receipt.run_id, 0, PayloadSlot::Output)
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
            crate::journal_fixture::execution(json!([{"kind":"RunCompleted"}])),
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
            .fixture_register(
                &app,
                leased_policy(
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
            matches!(&execution.outcomes[1], StepOutcome::RunFailed{ordinal:Some(1), name:Some(name), name_occurrence:0, error, max_attempts:1}
                if name == "large"
                    && error["type"] == "LimitExceededError"
                    && error["retryable"] == false),
            "revision {revision}: {:?}",
            execution.outcomes[1]
        );
        let receipt = tasks
            .complete(&task.id, &task.token, execution)
            .await
            .unwrap();
        // The refused step owns a journal row, so the run is forward work
        // rather than the platform's verdict on it.
        assert_eq!(receipt.state, RunState::Queued);
        assert_eq!(
            scope
                .payloads(&objects)
                .read_step_output(&task.invocation.run_id, "saved", 0)
                .await
                .unwrap()
                .into_bytes(256)
                .await
                .unwrap(),
            br#""prefix""#
        );
        assert_eq!(stored_rows(store.as_ref(), &task).await, 0);

        // What the body is handed next names the step that busted the budget.
        let resumed = tasks.poll().await.unwrap().unwrap();
        assert_eq!(resumed.invocation.run_id, task.invocation.run_id);
        let refused = resumed
            .invocation
            .journal
            .iter()
            .find(|entry| entry.ordinal == 1)
            .unwrap_or_else(|| panic!("{:?}", resumed.invocation.journal));
        assert_eq!(
            (refused.name.as_str(), refused.kind.as_str(), refused.state.as_str()),
            ("large", "run", "failed")
        );
        let error = refused.error.clone().unwrap();
        assert_eq!(error["type"], "LimitExceededError");
        // A body that does not catch it still rests the run on the same class.
        tasks
            .complete(
                &resumed.id,
                &resumed.token,
                crate::journal_fixture::execution(json!([{"kind":"RunFailed","error":error}])),
            )
            .await
            .unwrap();
        assert_eq!(
            scope.status(&task.invocation.run_id).await.unwrap().state,
            RunState::Failed
        );
    }

    // The control for the three cases above, differing in one thing: a
    // `sideEffect` output over the same budget. The journal records a named
    // failure as `kind: "run"`, so there is no row to name this step with and
    // the refusal stays the run's verdict.
    let task = claim(&scope, &tasks).await;
    let execution = prepare(
        &task,
        json!([{"kind":"StepCompleted","ordinal":0,"name":"effect",
                "stepKind":"sideEffect","output":"x".repeat(64)}]),
    )
    .stage(&tasks)
    .await
    .unwrap();
    assert!(
        matches!(
            &execution.outcomes[..],
            [StepOutcome::RunFailed {
                ordinal: None,
                name: None,
                ..
            }]
        ),
        "{:?}",
        execution.outcomes
    );
    assert_eq!(
        tasks
            .complete(&task.id, &task.token, execution)
            .await
            .unwrap()
            .state,
        RunState::Failed
    );

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
    // No step owns a whole runtime result, so this one names none.
    assert!(matches!(
        &result.outcomes[..],
        [StepOutcome::RunFailed {
            ordinal: None,
            name: None,
            ..
        }]
    ));
    tasks.complete(&task.id, &task.token, result).await.unwrap();
}

/// The startup half of the ceiling invariant: a configured host budget that
/// cannot carry what admission may grant a policy refuses before this host
/// serves. The admission half, which refuses a policy above the same constant,
/// lives with `AppPolicy`.
#[test]
fn configured_host_budget_refuses_a_payload_read_below_the_platform_ceiling() {
    // The derived default is at the ceiling, which is what makes the worker's
    // budget correct without a second literal to keep in step.
    let default = TaskPayloadLimits::default();
    assert_eq!(default.max_payload_bytes, MAX_PAYLOAD_BYTES_CEILING);
    default.validate_configured().unwrap();
    let at_ceiling = TaskPayloadLimits {
        max_inline_bytes: 1024,
        max_payload_bytes: MAX_PAYLOAD_BYTES_CEILING,
        max_result_bytes: MAX_PAYLOAD_BYTES_CEILING,
    };
    at_ceiling.validate_configured().unwrap();
    // A host may carry more than the platform admits; only less is refused.
    TaskPayloadLimits {
        max_payload_bytes: MAX_PAYLOAD_BYTES_CEILING + 1,
        max_result_bytes: MAX_PAYLOAD_BYTES_CEILING + 1,
        ..at_ceiling
    }
    .validate_configured()
    .unwrap();
    let below = TaskPayloadLimits {
        max_payload_bytes: MAX_PAYLOAD_BYTES_CEILING - 1,
        ..at_ceiling
    };
    // The control: this budget is internally consistent and passes every check
    // that compares it against itself, so the ceiling comparison is the only
    // thing that can account for the refusal.
    below.validate().unwrap();
    assert!(below.validate_configured().is_err());
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
