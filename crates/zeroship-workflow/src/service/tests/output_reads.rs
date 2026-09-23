use super::*;
use crate::backend::WorkflowBackend;
use crate::{
    operations::{RestartOptions, RunOperation},
    service::{StepOutput, WorkerIdentity},
};

#[compio::test]
async fn sqlite_named_outputs_follow_the_current_generation_without_object_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    output_contract(Rc::new(sqlite_store(&path).await)).await;
}

#[compio::test]
async fn postgres_named_outputs_follow_the_current_generation_without_object_storage() {
    let fixture = PostgresFixture::start().await;
    output_contract(Rc::new(fixture.store.clone())).await;
}

async fn output_contract(store: Rc<OrmStore>) {
    let (service, app, other, _deployments) = registered_service(store).await;
    let app = service.fixture_app(app);
    let other = service.fixture_app(other);
    let objects = objects::Objects::new();
    let backend = app
        .clone()
        .into_backend(
            &service,
            objects::StepOutputs::shared(&objects, 1024),
            objects.stager(),
        )
        .unwrap();
    assert_eq!(backend.app_id(), app.app_id());
    let worker = WorkerIdentity::new("output-reader".into()).unwrap();
    let run = backend
        .start("Example".into(), serde_json::Value::Null, StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service.complete(&worker, &task.id, &task.token, execution(json!([
        {"kind":"StepCompleted", "ordinal":0, "name":"value", "nameOccurrence":0, "output":{"value":1}},
        {"kind":"StepCompleted", "ordinal":1, "name":"value", "nameOccurrence":1, "output":{"value":2}},
        {"kind":"Wait", "ordinal":2, "name":"pending", "signalType":"ready"}
    ]))).await.unwrap();
    for (occurrence, value) in [(0, 1), (1, 2)] {
        // A step small enough to stay in the journal is answered from it, and
        // the object store is never asked.
        let StepOutput::Inline(read) = app
            .read_step_output(&run.id, "value", occurrence, objects.open())
            .await
            .unwrap()
        else {
            panic!("step output left the journal");
        };
        assert_eq!(read, json!({"value":value}));
        assert_eq!(
            backend
                .read_step_output(run.id.clone(), "value".into(), occurrence)
                .await
                .unwrap(),
            serde_json::to_vec(&read).unwrap()
        );
    }
    assert!(matches!(
        other
            .read_step_output(&run.id, "value", 0, objects.open())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    for (name, occurrence) in [("missing", 0), ("pending", 0), ("value", 2)] {
        assert!(matches!(
            app.read_step_output(&run.id, name, occurrence, objects.open())
                .await,
            Err(WorkflowServiceError::NotFound(_))
        ));
    }
    for (name, occurrence) in [("", 0), ("value", u32::MAX)] {
        assert!(matches!(
            app.read_step_output(&run.id, name, occurrence, objects.open())
                .await,
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }

    backend
        .transition(run.id.clone(), RunOperation::Cancel)
        .await
        .unwrap();
    backend
        .restart(run.id.clone(), RestartOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        app.read_step_output(&run.id, "value", 0, objects.open())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted", "ordinal":0, "name":"value", "output":null},
                {"kind":"RunCompleted"}
            ])),
        )
        .await
        .unwrap();
    let StepOutput::Inline(read) = app
        .read_step_output(&run.id, "value", 0, objects.open())
        .await
        .unwrap()
    else {
        panic!("step output left the journal");
    };
    assert_eq!(read, serde_json::Value::Null);
    assert_eq!(
        backend
            .read_step_output(run.id.clone(), "value".into(), 0)
            .await
            .unwrap(),
        b"null"
    );
}
