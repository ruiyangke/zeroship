use super::*;
use crate::operations::{RunOperation, RunState};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
mod failures;
mod schedules;

fn image(value: &str) -> Sources {
    Sources {
        entry: "entry.js".into(),
        modules: [
            (
                "entry.js".into(),
                "import value from './dependency.js'; export default value;".into(),
            ),
            (
                "dependency.js".into(),
                format!("export default {};", serde_json::to_string(value).unwrap()),
            ),
        ]
        .into(),
        descriptor: Some(json!({"collections": {}})),
    }
}
fn deployment(hash: char) -> DeployRegistration {
    DeployRegistration {
        id: typed_id::generate("dep"),
        hash: hash.to_string().repeat(64),
        workflows: ["Example".into()].into(),
        schedules: vec![],
    }
}

#[compio::test]
async fn sqlite_deployments_survive_redeploy_restart_corruption_and_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    deployment_contract(Rc::new(sqlite_store(&path).await), Deployments::new().await).await;
}

#[compio::test]
async fn postgres_deployments_survive_redeploy_restart_corruption_and_repair() {
    let fixture = PostgresFixture::start().await;
    deployment_contract(Rc::new(fixture.store.clone()), Deployments::new().await).await;
}

#[compio::test]
async fn normal_app_deployments_load_from_s3() {
    let fixture = s3_fixture::Minio::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    deployment_contract(
        Rc::new(sqlite_store(&path).await),
        Deployments::with_source(
            Arc::new(tempfile::tempdir().unwrap()),
            Arc::new(zeroship_bundle::S3BlobStore::new(
                fixture.config("deployments"),
                fixture.credentials(),
                1,
            )),
        )
        .await,
    )
    .await;
}

#[expect(
    clippy::too_many_lines,
    reason = "exercise the ordered retention, corruption and recovery contract"
)]
#[expect(
    clippy::future_not_send,
    reason = "the database and storage contract runs on its compio thread"
)]
async fn deployment_contract(store: Rc<OrmStore>, deployments: Deployments) {
    use crate::service::{runner::TaskTransport, WorkerIdentity};
    let (service, app, other, deployments) =
        registered_with_deployments(store.clone(), deployments).await;
    let original = image("original");
    let replacement = image("replacement");
    let first = deployments
        .publish(&app, &deployment('b'), &original)
        .await
        .unwrap();
    let second = deployments
        .publish(&app, &deployment('c'), &replacement)
        .await
        .unwrap();
    let foreign_deploy = deployments
        .publish(&other, &deployment('d'), &replacement)
        .await
        .unwrap();
    service.activate_deploy(&app, &first).await.unwrap();
    let scope = service.for_app(app.clone());
    let old_run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service.activate_deploy(&app, &second).await.unwrap();
    let new_run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    // Foreign app manifests remain independently scoped.
    service
        .activate_deploy(&other, &foreign_deploy)
        .await
        .unwrap();
    let other_run = service
        .for_app(other.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let reopened = WorkflowService::open(store.clone(), service.policies.clone())
        .await
        .unwrap()
        .with_deployments(service.deployments.clone().unwrap());
    let tasks = reopened.tasks(WorkerIdentity::new("customer".into()).unwrap());
    let foreign = reopened.tasks(WorkerIdentity::new("foreign".into()).unwrap());
    let mut active = Vec::new();
    while let Some(task) = tasks.poll().await.unwrap() {
        active.push(task);
    }
    assert_eq!(active.len(), 3);
    for task in &active {
        let expected = if task.invocation.run_id == old_run.id {
            &original
        } else {
            &replacement
        };
        expected.assert_loaded(&tasks.executable(task).await.unwrap());
        assert!(foreign.executable(task).await.is_err());
        assert!(
            matches!(task.invocation.run_id.as_str(), id if id == old_run.id || id == new_run.id || id == other_run.id)
        );
    }
    assert!(matches!(
        service
            .activate_deploy(
                &app,
                &DeployRegistration {
                    workflows: ["Other".into()].into(),
                    ..first.clone()
                }
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let old_task = active
        .iter()
        .find(|task| task.invocation.run_id == old_run.id)
        .unwrap();
    original.assert_loaded(&tasks.executable(old_task).await.unwrap());
    tasks
        .complete(
            &old_task.id,
            &old_task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    assert!(tasks.executable(old_task).await.is_err());
    let new_task = active
        .iter()
        .find(|task| task.invocation.run_id == new_run.id)
        .unwrap();
    let objects = &deployments.source;
    let bytes = objects
        .get_manifest(&app.uuid(), &second.hash)
        .await
        .unwrap();
    let mut corrupt = bytes.to_vec();
    *corrupt.last_mut().unwrap() ^= 1;
    objects
        .delete_manifest(&app.uuid(), &second.hash)
        .await
        .unwrap();
    objects
        .put_manifest(&app.uuid(), &second.hash, &corrupt)
        .await
        .unwrap();
    assert!(matches!(
        tasks.executable(new_task).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    tasks.release(&new_task.id, &new_task.token).await.unwrap();
    assert!(
        tasks.poll().await.unwrap().is_none(),
        "corrupt deployment was dispatched again"
    );
    assert!(scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .is_err());
    service.activate_deploy(&app, &first).await.unwrap();
    let unaffected = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = tasks.poll().await.unwrap().unwrap();
    assert_eq!(
        task.invocation.run_id, unaffected.id,
        "parked work starved another deployment"
    );
    tasks
        .complete(
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    objects
        .delete_manifest(&app.uuid(), &second.hash)
        .await
        .unwrap();
    objects
        .put_manifest(&app.uuid(), &second.hash, &bytes)
        .await
        .unwrap();
    service.retain_deploy(&app, &second).await.unwrap();
    let recovered = tasks.poll().await.unwrap().unwrap();
    assert_eq!(recovered.invocation.run_id, new_run.id);
    replacement.assert_loaded(&tasks.executable(&recovered).await.unwrap());
    let post_repair = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let post_repair_task = tasks.poll().await.unwrap().unwrap();
    assert_eq!(post_repair_task.invocation.run_id, post_repair.id);
    assert_eq!(
        post_repair_task.invocation.deploy_id, first.id,
        "repair changed the active deployment"
    );
    tasks
        .complete(
            &post_repair_task.id,
            &post_repair_task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    objects
        .delete_manifest(&app.uuid(), &second.hash)
        .await
        .unwrap();
    assert!(matches!(
        tasks.executable(&recovered).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    let mut tx = reopened.begin().await.unwrap();
    let expired = tx.now().await.unwrap() - 1;
    tx.execute(
        &format!("UPDATE {} SET deadline=$2 WHERE id=$1", tx.table("tasks")),
        &[recovered.id.clone().into(), expired.into()],
    )
    .await
    .unwrap();
    tx.execute(
        &format!(
            "UPDATE {} SET due_at=$3 WHERE app_id=$1 AND id=$2",
            tx.table("runs")
        ),
        &[
            app.as_str().into(),
            new_run.id.clone().into(),
            expired.into(),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(tasks.poll().await.unwrap().is_none());
    let mut tx = reopened.begin().await.unwrap();
    let rows = tx
        .query(
            &format!("SELECT state FROM {} WHERE id=$1", tx.table("tasks")),
            &[recovered.id.clone().into()],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].text("state").unwrap(), "expired");
    tx.commit().await.unwrap();
    // Corruption or deletion in A's scope must leave B's bytes intact.
    let other_task = active
        .iter()
        .find(|task| task.invocation.run_id == other_run.id)
        .unwrap();
    replacement.assert_loaded(&tasks.executable(other_task).await.unwrap());
    scope
        .transition(&RequestId::mint(), &new_run.id, RunOperation::Cancel)
        .await
        .unwrap();
    assert!(tasks.poll().await.unwrap().is_none());
    assert_eq!(
        scope.status(&new_run.id).await.unwrap().state,
        RunState::Cancelled
    );
}

#[test]
fn artifact_binding_rejects_an_empty_source_budget() {
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(LocalDiskBlobStore::new(dir.path().into()).unwrap());
    assert!(super::super::AppDeployments::new(source, 0).is_err());
}
