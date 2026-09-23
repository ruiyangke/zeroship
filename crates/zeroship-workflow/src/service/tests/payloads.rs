use super::objects::Objects;
use super::*;
use crate::{
    engine::WorkflowOutputRef,
    operations::RunState,
    service::{PayloadSlot, StepOutput, WorkerIdentity},
};
use std::rc::Rc;

pub(super) mod collection;
mod ownerless;

fn reference(value: &[u8]) -> WorkflowOutputRef {
    WorkflowOutputRef {
        hash: crate::service::types::hash(value),
        size: value.len() as i64,
        content_type: Some("application/json".into()),
    }
}

#[compio::test]
async fn sqlite_payload_ownership_and_retention() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    payload_contract(Rc::new(sqlite_store(&path).await), Objects::new()).await;
}

#[compio::test]
async fn postgres_payload_ownership_and_retention() {
    let fixture = PostgresFixture::start().await;
    payload_contract(Rc::new(fixture.store.clone()), Objects::new()).await;
}

#[compio::test]
async fn sqlite_siblings_started_from_one_object_each_own_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    shared_child_input(Rc::new(sqlite_store(&path).await)).await;
}

#[compio::test]
async fn postgres_siblings_started_from_one_object_each_own_it() {
    let fixture = PostgresFixture::start().await;
    shared_child_input(Rc::new(fixture.store.clone())).await;
}

/// Two children started from byte-identical inputs share one object, and each
/// one owns it.
///
/// Preparation deduplicates uploads by descriptor, so a `startMany` over
/// identical items stages the bytes once. The first child's acceptance then
/// puts that object's only edge on the CHILD, where no edge of the parent's
/// reaches it -- so what proves the second child may take it is the staging the
/// parent's own task did, which the object's columns still record.
async fn shared_child_input(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let objects = Objects::new();
    let worker = WorkerIdentity::new("shared-child-input".into()).unwrap();
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let parent = service.poll(&worker).await.unwrap().unwrap();
    let data = br#"{"item":1}"#;
    let input = reference(data);
    service
        .stage_payload(
            &worker,
            &parent.id,
            &parent.token,
            &RequestId::mint(),
            input.clone(),
            objects.upload(data),
        )
        .await
        .unwrap();
    service
        .complete(
            &worker,
            &parent.id,
            &parent.token,
            execution(json!([
                {"kind":"Child","ordinal":0,"name":"item-0","childWorkflowName":"Child",
                 "inputRef":input, "options":{}},
                {"kind":"Child","ordinal":1,"name":"item-1","childWorkflowName":"Child",
                 "inputRef":input, "options":{}},
            ])),
        )
        .await
        .unwrap();
    let mut started = Vec::new();
    while let Some(child) = service.poll(&worker).await.unwrap() {
        assert_eq!(child.invocation.workflow_name, "Child");
        assert!(child.invocation.trigger.input.is_none());
        assert_eq!(child.invocation.trigger.input_ref, Some(input.clone()));
        assert_eq!(
            service
                .read_task_payload(&worker, &child.id, &child.token, &input, objects.open())
                .await
                .unwrap(),
            data
        );
        started.push(child.invocation.run_id.clone());
    }
    assert_eq!(started.len(), 2, "both children must start: {started:?}");

    // Each child owns the object in its own right, so retiring one leaves the
    // other's input intact. Without both edges the shared object would belong
    // to whichever sibling happened to be admitted first.
    let tx = service.begin().await.unwrap();
    let mut owners: Vec<String> = journal_rows(
        &tx,
        "payload_refs",
        json!({"app_id":app.as_str(), "slot":"input"}),
    )
    .await
    .iter()
    .map(|row| row.text("run_id").unwrap())
    .collect();
    tx.commit().await.unwrap();
    owners.sort();
    started.sort();
    assert_eq!(owners, started);
}

#[compio::test]
async fn postgres_payload_record_write_that_outlives_its_lease_rolls_back() {
    delayed_payload_write("INSERT").await;
}

#[compio::test]
async fn postgres_payload_confirmation_write_that_outlives_its_lease_rolls_back() {
    delayed_payload_write("UPDATE").await;
}

async fn delayed_payload_write(operation: &str) {
    let fixture = PostgresFixture::start().await;
    let store: Rc<OrmStore> = Rc::new(fixture.store.clone());
    let (service, app, _, _deployments) = registered_service(store.clone()).await;
    let objects = Objects::new();
    service
        .fixture_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .fixture_register(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    lease_ms: 300,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    let admin = connect(&fixture.admin_url).await;
    admin.batch_execute(&format!("CREATE SEQUENCE customer.delayed_payload_writes; GRANT USAGE ON SEQUENCE customer.delayed_payload_writes TO app_customer_role; CREATE FUNCTION customer.delay_payload_write() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('customer.delayed_payload_writes'); PERFORM pg_sleep(0.5); RETURN NEW; END $$; CREATE TRIGGER delay_payload_write BEFORE {operation} ON customer.__zeroship_workflow_payloads FOR EACH ROW EXECUTE FUNCTION customer.delay_payload_write();")).await.unwrap();
    let worker = WorkerIdentity::new("payload-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let result = service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            reference(b"delayed"),
            objects.upload(b"delayed"),
        )
        .await;
    assert!(
        matches!(result, Err(WorkflowServiceError::Conflict(_))),
        "{result:?}"
    );
    assert!(admin
        .query_one("SELECT is_called FROM customer.delayed_payload_writes", &[])
        .await
        .unwrap()
        .get::<_, bool>(0));
    let tx = store.begin().await.unwrap();
    let rows = journal_rows(
        &tx,
        "payloads",
        json!({"app_id":app.as_str(), "task_id":task.id}),
    )
    .await;
    if operation == "INSERT" {
        assert!(
            rows.is_empty(),
            "expired upload admission must roll back its record"
        );
    } else {
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text("state").unwrap(), "uploading");
    }
    tx.commit().await.unwrap();
    admin.batch_execute("DROP TRIGGER delay_payload_write ON customer.__zeroship_workflow_payloads; UPDATE customer.__zeroship_workflow_payloads SET expires_at=0;").await.unwrap();
    assert_eq!(
        service.collect_payloads(1, &objects).await.unwrap(),
        usize::from(operation == "UPDATE")
    );
}

async fn payload_contract(store: Rc<OrmStore>, objects: Objects) {
    let (service, a, b, _deployments) = registered_service(store.clone()).await;
    let scope = service.fixture_app(a.clone());
    let foreign = service.fixture_app(b);
    let worker = WorkerIdentity::new("payload-worker".into()).unwrap();
    let stranger = WorkerIdentity::new("stranger".into()).unwrap();
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let data = br#"{"value":"retained"}"#;
    let output = reference(data);
    let request = RequestId::mint();
    assert!(matches!(
        service
            .stage_payload(
                &stranger,
                &task.id,
                &task.token,
                &request,
                output.clone(),
                objects.upload(data)
            )
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let staged = service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &request,
            output.clone(),
            objects.upload(data),
        )
        .await
        .unwrap();
    assert_eq!(
        staged,
        service
            .stage_payload(
                &worker,
                &task.id,
                &task.token,
                &request,
                output.clone(),
                objects.upload(b"ignored retry body")
            )
            .await
            .unwrap()
    );
    assert!(matches!(
        service
            .stage_payload(
                &worker,
                &task.id,
                &task.token,
                &request,
                reference(b"another"),
                objects.upload(b"another")
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert_eq!(
        service
            .read_task_payload(&worker, &task.id, &task.token, &output, objects.open())
            .await
            .unwrap(),
        data
    );
    assert!(matches!(
        scope
            .read_payload(&run.id, 0, PayloadSlot::Output, objects.open())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        foreign
            .read_payload(&run.id, 0, PayloadSlot::Output, objects.open())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    for other in [&foreign, &scope] {
        other
            .start(&RequestId::mint(), "Child", StartOptions::default())
            .await
            .unwrap();
        let other_task = service.poll(&worker).await.unwrap().unwrap();
        assert_ne!(other_task.id, task.id);
        assert!(matches!(
            service
                .read_task_payload(
                    &worker,
                    &other_task.id,
                    &other_task.token,
                    &output,
                    objects.open()
                )
                .await,
            Err(WorkflowServiceError::NotFound(_))
        ));
        assert!(matches!(
            service
                .complete(
                    &worker,
                    &other_task.id,
                    &other_task.token,
                    execution(json!([{"kind":"RunCompleted","outputRef":output}]))
                )
                .await,
            Err(WorkflowServiceError::NotFound(_))
        ));
        service
            .complete(
                &worker,
                &other_task.id,
                &other_task.token,
                execution(json!([{"kind":"RunCompleted"}])),
            )
            .await
            .unwrap();
    }
    // A mismatched body never becomes an admissible descriptor.
    let wrong = reference(b"x");
    let interrupted = RequestId::mint();
    assert!(matches!(
        service
            .stage_payload(
                &worker,
                &task.id,
                &task.token,
                &interrupted,
                wrong.clone(),
                objects.upload(b"y")
            )
            .await,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    assert!(matches!(
        service
            .read_task_payload(&worker, &task.id, &task.token, &wrong, objects.open())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &interrupted,
            wrong.clone(),
            objects.upload(b"x"),
        )
        .await
        .unwrap();
    let uncommitted = service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            reference(b"uncommitted"),
            objects.upload(b"uncommitted"),
        )
        .await
        .unwrap();
    // A late invalid frontier rolls back the earlier payload promotion.
    assert!(service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted","ordinal":0,"name":"result","outputRef":output},
                {"kind":"StepCompleted","ordinal":9,"name":"invalid"}
            ]))
        )
        .await
        .is_err());
    let tx = store.begin().await.unwrap();
    let state = journal_rows(
        &tx,
        "payloads",
        json!({"app_id":a.as_str(), "id":staged.id}),
    )
    .await;
    assert_eq!(state[0].text("state").unwrap(), "staged");
    tx.commit().await.unwrap();
    let completion = execution(json!([
        {"kind":"StepCompleted","ordinal":0,"name":"result","outputRef":output},
        {"kind":"Wait","ordinal":1,"name":"approval","signalType":"approved"}
    ]));
    service
        .complete(&worker, &task.id, &task.token, completion.clone())
        .await
        .unwrap();
    service
        .complete(&worker, &task.id, &task.token, completion)
        .await
        .unwrap();
    expire_uploads(&store).await;
    assert!(service.collect_payloads(64, &objects).await.unwrap() > 0);
    assert_eq!(service.collect_payloads(64, &objects).await.unwrap(), 0);
    assert_eq!(
        scope
            .read_step_output(&run.id, "result", 0, objects.open())
            .await
            .unwrap(),
        StepOutput::Object(data.to_vec())
    );
    assert!(matches!(
        foreign
            .read_step_output(&run.id, "result", 0, objects.open())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(!objects.exists(&a, &uncommitted.id));
    // Simulate an already-sent remote upload arriving after its writer died.
    objects.put(&a, &uncommitted.id, b"uncommitted");
    expire_uploads(&store).await;
    service.collect_payloads(64, &objects).await.unwrap();
    assert!(!objects.exists(&a, &uncommitted.id));
    assert_eq!(
        scope
            .read_payload(&run.id, 0, PayloadSlot::Step { ordinal: 0 }, objects.open())
            .await
            .unwrap(),
        data
    );
    scope
        .restart(
            &RequestId::mint(),
            &run.id,
            crate::operations::RestartOptions {
                from: Some(crate::operations::RestartTarget {
                    name: "approval".into(),
                    occurrence: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let recovered = WorkflowService::open(store.clone(), service.policies.clone())
        .await
        .unwrap();
    let next = recovered.poll(&worker).await.unwrap().unwrap();
    assert_eq!(next.generation, 1);
    assert_eq!(next.invocation.app_id, a.as_str());
    assert_eq!(next.invocation.run_id, run.id);
    assert_eq!(
        recovered
            .read_task_payload(
                &worker,
                &next.id,
                &next.token,
                &reference(data),
                objects.open()
            )
            .await
            .unwrap(),
        data
    );
    assert!(matches!(
        scope
            .read_step_output(&run.id, "missing", 0, objects.open())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        scope
            .read_step_output(&run.id, "result", 1, objects.open())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert_eq!(
        scope
            .read_step_output(&run.id, "result", 0, objects.open())
            .await
            .unwrap(),
        StepOutput::Object(data.to_vec())
    );
    assert_eq!(
        next.invocation.journal[0].output_ref.as_ref(),
        Some(&output)
    );
    assert_eq!(
        recovered
            .read_task_payload(&worker, &next.id, &next.token, &output, objects.open())
            .await
            .unwrap(),
        data
    );
    assert!(recovered
        .read_task_payload(&worker, &task.id, &task.token, &output, objects.open())
        .await
        .is_err());
    assert!(recovered
        .read_task_payload(
            &worker,
            &next.id,
            &next.token,
            &uncommitted.reference,
            objects.open()
        )
        .await
        .is_err());
    recovered
        .complete(
            &worker,
            &next.id,
            &next.token,
            execution(json!([{"kind":"RunCompleted","outputRef":output}])),
        )
        .await
        .unwrap();
    // The whole descriptor, not just the hash: the status reply crosses into V8
    // through `dispatch_json`, so these fields and this `kind` are the entire
    // contract a creator narrows on. `StatusOutputRef` in
    // `packages/workflows/src/index.ts` declares the same shape, and nothing
    // mechanical ties the two literals together.
    assert_eq!(
        scope.status(&run.id).await.unwrap().output.unwrap(),
        json!({
            "kind": "ref",
            "ref": format!("wfblob:sha256:{}", output.hash),
            "hash": output.hash,
            "size": output.size,
            "contentType": output.content_type,
        })
    );
    assert!(recovered
        .read_task_payload(
            &worker,
            &next.id,
            &next.token,
            &reference(data),
            objects.open()
        )
        .await
        .is_err());
    assert_eq!(
        scope
            .read_payload(&run.id, 1, PayloadSlot::Output, objects.open())
            .await
            .unwrap(),
        data
    );

    continuation_and_child(&recovered, &a, &worker, &objects).await;
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let quota_task = recovered.poll(&worker).await.unwrap().unwrap();
    let policy = AppPolicy {
        max_payload_bytes: 1,
        max_payload_objects: 1,
        ..Default::default()
    };
    recovered
        .fixture_register(&a, super::leased_policy(2, policy))
        .await
        .unwrap();
    assert!(matches!(
        recovered
            .stage_payload(
                &worker,
                &quota_task.id,
                &quota_task.token,
                &RequestId::mint(),
                output.clone(),
                objects.upload(data)
            )
            .await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));
    assert!(matches!(
        recovered
            .stage_payload(
                &worker,
                &quota_task.id,
                &quota_task.token,
                &RequestId::mint(),
                reference(b""),
                objects.upload(b"")
            )
            .await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    recovered
        .fixture_register(&a, super::leased_policy(3, AppPolicy::default()))
        .await
        .unwrap();
    recovered
        .complete(
            &worker,
            &quota_task.id,
            &quota_task.token,
            execution(json!([{ "kind":"RunCompleted" }])),
        )
        .await
        .unwrap();
    expire_uploads(&store).await;
    recovered.collect_payloads(64, &objects).await.unwrap();
    assert_eq!(
        scope
            .read_payload(&run.id, 0, PayloadSlot::Step { ordinal: 0 }, objects.open())
            .await
            .unwrap(),
        data
    );
}

async fn expire_uploads(store: &Rc<OrmStore>) {
    let tx = store.begin().await.unwrap();
    journal_update(&tx, "payloads", json!({}), json!({"expires_at":0})).await;
    tx.commit().await.unwrap();
}

async fn continuation_and_child(
    service: &WorkflowService,
    app: &AppId,
    worker: &WorkerIdentity,
    objects: &Objects,
) {
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(worker).await.unwrap().unwrap();
    service.complete(worker, &task.id, &task.token, execution(json!([{"kind":"Child","ordinal":0,"name":"child","childWorkflowName":"Child","options":{}}]))).await.unwrap();
    let child = service.poll(worker).await.unwrap().unwrap();
    let data = br#"{"continued":true}"#;
    let output = reference(data);
    service
        .stage_payload(
            worker,
            &child.id,
            &child.token,
            &RequestId::mint(),
            output.clone(),
            objects.upload(data),
        )
        .await
        .unwrap();
    service
        .complete(
            worker,
            &child.id,
            &child.token,
            execution(json!([{"kind":"ContinueAsNew","inputRef":output}])),
        )
        .await
        .unwrap();
    let successor = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(successor.invocation.trigger.input_ref, Some(output.clone()));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &service
                .read_task_payload(
                    worker,
                    &successor.id,
                    &successor.token,
                    &output,
                    objects.open()
                )
                .await
                .unwrap()
        )
        .unwrap(),
        json!({"continued":true})
    );
    assert_eq!(
        service
            .read_task_payload(
                worker,
                &successor.id,
                &successor.token,
                &output,
                objects.open()
            )
            .await
            .unwrap(),
        data
    );
    service
        .complete(
            worker,
            &successor.id,
            &successor.token,
            execution(json!([{"kind":"RunCompleted","outputRef":output}])),
        )
        .await
        .unwrap();
    assert_eq!(deliver_propagations(&scope).await.len(), 1);
    let parent = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(parent.invocation.run_id, task.invocation.run_id);
    assert_eq!(
        parent.invocation.journal[0].output_ref,
        Some(output.clone())
    );
    assert_eq!(
        service
            .read_task_payload(worker, &parent.id, &parent.token, &output, objects.open())
            .await
            .unwrap(),
        data
    );
    let done = service
        .complete(
            worker,
            &parent.id,
            &parent.token,
            execution(json!([{"kind":"RunCompleted","outputRef":output}])),
        )
        .await
        .unwrap();
    assert_eq!(done.state, RunState::Completed);
}

#[compio::test]
async fn deletion_failure_recovers_without_reopening_payload_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    let store: Rc<OrmStore> = Rc::new(sqlite_store(&path).await);
    let (service, app, _, _deployments) = registered_service(store.clone()).await;
    let objects = Objects::new();
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let request = RequestId::mint();
    let output = reference(b"abandoned");
    let staged = service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &request,
            output.clone(),
            objects.upload(b"abandoned"),
        )
        .await
        .unwrap();
    objects.fail(&staged.id);
    expire_uploads(&store).await;
    assert!(service.collect_payloads(64, &objects).await.is_err());
    assert!(service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &request,
            output.clone(),
            objects.upload(b"abandoned")
        )
        .await
        .is_err());
    assert!(matches!(
        service
            .complete(
                &worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"RunCompleted","outputRef":output}]))
            )
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let recovered = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap();
    assert_eq!(recovered.collect_payloads(64, &objects).await.unwrap(), 1);
    assert!(!objects.exists(&app, &staged.id));
    recovered
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
}

#[compio::test]
async fn postgres_collection_rechecks_references_after_waiting_for_completion() {
    use std::time::Duration;

    let fixture = PostgresFixture::start().await;
    let store: Rc<OrmStore> = Rc::new(fixture.store.clone());
    let (service, app, _, _deployments) = registered_service(store.clone()).await;
    let objects = Objects::new();
    let collector = WorkflowService::open(
        Rc::new(
            orm_store(
                &fixture
                    .admin_url
                    .replacen("postgres@", "customer_worker@", 1),
                fixture.store.binding.schema().clone(),
            )
            .await,
        ),
        service.policies.clone(),
    )
    .await
    .unwrap()
    .with_deployments(service.deployments.clone().unwrap());
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let output = reference(b"survives");
    let staged = service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            output.clone(),
            objects.upload(b"survives"),
        )
        .await
        .unwrap();
    let admin = connect(&fixture.admin_url).await;
    admin.batch_execute("CREATE FUNCTION customer.gate_payload_promotion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.slot='output' THEN PERFORM pg_advisory_xact_lock(73921862); END IF; RETURN NEW; END $$; CREATE TRIGGER gate_payload_promotion BEFORE INSERT ON customer.__zeroship_workflow_payload_refs FOR EACH ROW EXECUTE FUNCTION customer.gate_payload_promotion();").await.unwrap();
    let blocker_pid: i32 = admin
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    admin
        .query_one("SELECT pg_advisory_lock(73921862)", &[])
        .await
        .unwrap();
    let completing_service = service.clone();
    let completing = compio::runtime::spawn(async move {
        completing_service
            .complete(
                &worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"RunCompleted","outputRef":output}])),
            )
            .await
    });
    let completion_pid = compio::time::timeout(Duration::from_secs(10), async {
        loop {
            let row = admin.query_one("SELECT min(pid) FROM pg_stat_activity WHERE usename='customer_worker' AND wait_event_type='Lock' AND position('__zeroship_workflow_payload_refs' in query) > 0 AND $1=ANY(pg_blocking_pids(pid))", &[&blocker_pid]).await.unwrap();
            if let Some(pid) = row.get::<_, Option<i32>>(0) { break pid; }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("completion reached the gated output reference insertion");
    // Completion has validated the staged payload and still owns the app lock.
    // Expire its committed record before the independent collector discovers it.
    assert_eq!(
        compio::time::timeout(
            Duration::from_secs(10),
            admin.execute(
                "UPDATE customer.__zeroship_workflow_payloads SET expires_at=0 WHERE app_id=$1 AND id=$2 AND state='staged'",
                &[&app.as_str(), &staged.id],
            ),
        )
        .await
        .expect("expire the staged payload before reference insertion")
        .unwrap(),
        1
    );
    let collecting = {
        let objects = objects.clone();
        compio::runtime::spawn(async move { collector.collect_payloads(64, &objects).await })
    };
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            let row = admin.query_one("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE usename='customer_worker' AND wait_event_type='Lock' AND position('__zeroship_workflow_app_state' in query) > 0 AND $1=ANY(pg_blocking_pids(pid)))", &[&completion_pid]).await.unwrap();
            if row.get::<_, bool>(0) { break; }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("collector discovered the expired payload and reached completion's app lock");
    assert!(admin
        .query_one("SELECT pg_advisory_unlock(73921862)", &[])
        .await
        .unwrap()
        .get::<_, bool>(0));
    compio::time::timeout(Duration::from_secs(10), async {
        let (completion, collected) = futures::join!(completing, collecting);
        completion.unwrap().unwrap();
        assert_eq!(collected.unwrap().unwrap(), 0);
    })
    .await
    .expect("completion and collection settled after releasing promotion");
    assert_eq!(
        scope
            .read_payload(&run.id, 0, PayloadSlot::Output, objects.open())
            .await
            .unwrap(),
        b"survives"
    );
}

#[compio::test]
async fn sqlite_a_run_input_answers_to_the_input_bound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    input_bound(Rc::new(sqlite_store(&path).await)).await;
}

#[compio::test]
async fn postgres_a_run_input_answers_to_the_input_bound() {
    let fixture = PostgresFixture::start().await;
    input_bound(Rc::new(fixture.store.clone())).await;
}

/// A run's input answers to `max_input_bytes`, not to the payload ceiling.
///
/// The executor reads the object in full and hands the body the value, so the
/// bytes are resident alongside the isolate for the whole execution whatever
/// held them. Staging moved where a run's input is stored; it does not move
/// which ceiling it answers to, and `max_payload_bytes` is the budget for a
/// blob read back on demand rather than one materialized at start.
///
/// Two refusals, because there are two ways in. Staging refuses a value before
/// an object is minted, so an oversized start spends none of the app's payload
/// budget. `insert_run` refuses a DESCRIPTOR, which is what an executor reaches
/// the service with: the runner admits a child's input and a continuation seed
/// against the payload ceiling, so the bound has to hold on a reference that
/// was staged somewhere this app's policy did not gate.
async fn input_bound(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let objects = Objects::new();
    let bound = AppPolicy::default().max_input_bytes;

    // A JSON string of n characters encodes to n + 2 bytes, so these land
    // exactly on the bound and exactly one byte over it.
    let at = serde_json::Value::String("a".repeat(bound - 2));
    let over = serde_json::Value::String("a".repeat(bound - 1));
    assert_eq!(crate::service::app::encode(&at).unwrap().len(), bound);
    assert_eq!(crate::service::app::encode(&over).unwrap().len(), bound + 1);

    // Staging: the control first, so the refusal below is the size and not the
    // path refusing everything handed to it.
    let staged = crate::service::payloads::stage_start_input(
        &objects,
        &scope,
        &RequestId::mint(),
        &at,
        bound,
    )
    .await
    .unwrap()
    .expect("a value at the bound stages");
    assert_eq!(staged.size, bound as i64);
    assert!(matches!(
        crate::service::payloads::stage_start_input(
            &objects,
            &scope,
            &RequestId::mint(),
            &over,
            bound,
        )
        .await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));

    // Nothing was minted for the refused value. The object count is the whole
    // record of that: refusing after staging would leave one behind.
    let tx = service.begin().await.unwrap();
    let objects_held = journal_rows(&tx, "payloads", json!({"app_id":app.as_str()}))
        .await
        .len();
    tx.commit().await.unwrap();
    assert_eq!(objects_held, 1, "only the admitted value may hold an object");

    // The descriptor: a run admitted from the staged object, then the same
    // start with a reference whose size exceeds the bound.
    let admitted = scope
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input_ref: Some(staged.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!admitted.id.is_empty());
    assert!(matches!(
        scope
            .start(
                &RequestId::mint(),
                "Example",
                StartOptions {
                    input_ref: Some(WorkflowOutputRef {
                        size: bound as i64 + 1,
                        ..staged
                    }),
                    ..Default::default()
                },
            )
            .await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));
}
