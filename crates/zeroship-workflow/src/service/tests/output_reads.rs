use super::*;
use crate::backend::WorkflowBackend;
use crate::{
    operations::{RestartOptions, RunOperation},
    service::WorkerIdentity,
};

#[compio::test]
async fn buffered_payload_reads_consume_integrity_verification_at_eof() {
    use crate::{engine::WorkflowOutputRef, service::PayloadRead};
    use zeroship_storage::backend::OnceChunk;
    let bytes = b"valid";
    for actual in [b"wrong".as_slice(), b"truncated", b""] {
        let reference = WorkflowOutputRef {
            hash: crate::service::types::hash(bytes),
            size: bytes.len() as i64,
            content_type: None,
        };
        let read =
            PayloadRead::checked(reference, Box::new(OnceChunk::new(actual.to_vec().into())))
                .unwrap();
        assert!(matches!(
            read.into_bytes(1024).await,
            Err(WorkflowServiceError::Unavailable(_))
        ));
    }
}

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
    let app = service.for_app(app);
    let other = service.for_app(other);
    let backend = app.clone().into_backend(1024).unwrap();
    assert_eq!(backend.app_id(), app.app_id());
    assert!(app.clone().into_backend(0).is_err());
    let worker = WorkerIdentity::new("output-reader".into()).unwrap();
    let run = backend
        .start("Example".into(), StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service.complete(&worker, &task.id, &task.token, execution(json!([
        {"kind":"StepCompleted", "ordinal":0, "name":"value", "nameOccurrence":0, "output":{"value":1}},
        {"kind":"StepCompleted", "ordinal":1, "name":"value", "nameOccurrence":1, "output":{"value":2}},
        {"kind":"Wait", "ordinal":2, "name":"pending", "signalType":"ready"}
    ]))).await.unwrap();
    for (occurrence, value) in [(0, 1), (1, 2)] {
        let read = app
            .read_step_output(&run.id, "value", occurrence)
            .await
            .unwrap();
        assert_eq!(
            read.reference.content_type.as_deref(),
            Some("application/json")
        );
        let bytes = read.into_bytes(1024).await.unwrap();
        assert_eq!(
            backend
                .read_step_output(run.id.clone(), "value".into(), occurrence)
                .await
                .unwrap(),
            bytes
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            json!({"value":value})
        );
    }
    assert!(matches!(
        other.read_step_output(&run.id, "value", 0).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    for (name, occurrence) in [("missing", 0), ("pending", 0), ("value", 2)] {
        assert!(matches!(
            app.read_step_output(&run.id, name, occurrence).await,
            Err(WorkflowServiceError::NotFound(_))
        ));
    }
    for (name, occurrence) in [("", 0), ("value", u32::MAX)] {
        assert!(matches!(
            app.read_step_output(&run.id, name, occurrence).await,
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
    let read = app.read_step_output(&run.id, "value", 0).await.unwrap();
    assert!(matches!(
        read.into_bytes(1).await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));

    backend
        .transition(run.id.clone(), RunOperation::Cancel)
        .await
        .unwrap();
    backend
        .restart(run.id.clone(), RestartOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        app.read_step_output(&run.id, "value", 0).await,
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
                {"kind":"RunCompleted", "output":"done"}
            ])),
        )
        .await
        .unwrap();
    let read = app.read_step_output(&run.id, "value", 0).await.unwrap();
    assert_eq!(read.into_bytes(1024).await.unwrap(), b"null");
}
