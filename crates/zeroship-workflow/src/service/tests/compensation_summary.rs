use super::*;
use crate::{operations::RunState, service::WorkerIdentity};

#[compio::test]
async fn sqlite_completed_rollback_reports_its_summary_on_the_original_error() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("workflow.sqlite")).await;
    rollback_summary(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_completed_rollback_reports_its_summary_on_the_original_error() {
    let fixture = PostgresFixture::start().await;
    rollback_summary(Rc::new(fixture.store.clone())).await;
}

/// A fully rolled back failure keeps the creator's error and reports the
/// rollback in `error.compensation`, the slot the workflows reference defines.
async fn rollback_summary(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let worker = WorkerIdentity::new("compensation-summary".into()).unwrap();
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted","ordinal":0,"name":"reserve","compensable":true,"output":0},
                {"kind":"RunFailed","error":{"type":"Error","message":"intentional failure"}}
            ])),
        )
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.phase, "compensating");
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"CompensationCompleted","ordinal":0,"name":"reserve"}])),
        )
        .await
        .unwrap();
    let status = scope.status(&run.id).await.unwrap();
    assert_eq!(status.state, RunState::Failed);
    let error = status.error.unwrap();
    assert_eq!(error["type"], json!("Error"));
    assert_eq!(error["message"], json!("intentional failure"));
    assert_eq!(
        error["compensation"],
        json!({"total":1, "completed":1, "failed":0, "outcome":"completed"})
    );
}
