use super::objects::Objects;
use super::*;
use crate::{
    engine::WorkflowOutputRef,
    service::{app, models, PayloadOpener, PayloadTarget, WorkerIdentity},
};
use zeroship_data_orm::{orm::Entity, value};

/// Hands back the descriptor admission resolved together with the bytes, so a
/// read states which object it reached rather than only what it holds.
struct Descriptor<'a>(&'a Objects);
#[async_trait::async_trait(?Send)]
impl PayloadOpener for Descriptor<'_> {
    type Read = (WorkflowOutputRef, Vec<u8>);
    async fn open(self, target: PayloadTarget<'_>) -> Result<Self::Read, WorkflowServiceError> {
        let reference: WorkflowOutputRef = target.reference.clone();
        Ok((reference, self.0.open().open(target).await?))
    }
}

#[compio::test]
async fn sqlite_payload_quota_uses_exact_scoped_aggregates() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    quota_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_payload_quota_uses_exact_scoped_aggregates() {
    let fixture = PostgresFixture::start().await;
    quota_contract(Rc::new(fixture.store.clone())).await;
}

async fn quota_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, _deployments) = registered_service(store).await;
    let objects = Objects::new();
    let worker = WorkerIdentity::new("payload-model-worker".into()).unwrap();
    let large = (1_i64 << 53) + 1;
    let mut local_task = None;
    for app_id in [&local, &foreign] {
        service
            .fixture_app(app_id.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
        let task = service.poll(&worker).await.unwrap().unwrap();
        assert_eq!(task.invocation.app_id, app_id.as_str());
        let mut tx = service.begin().await.unwrap();
        app::lock_app(&mut tx, app_id).await.unwrap();
        for (state, size) in [("staged", large), ("deleted", i64::MAX)] {
            tx.database().collection(models::payloads::Entity::COLLECTION).unwrap().insert(value!({
                "app_id":app_id.as_str(), "run_id":task.invocation.run_id.clone(), "generation":task.generation,
                "id":typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX), "task_id":task.id.clone(),
                "request_id":RequestId::mint().as_str(), "hash":"0".repeat(64), "size":size,
                "content_type":null, "state":state, "created_at":0, "expires_at":i64::MAX,
            })).await.unwrap();
        }
        tx.commit().await.unwrap();
        if app_id == &local {
            local_task = Some(task);
        }
    }
    service
        .fixture_register(
            &local,
            leased_policy(
                2,
                AppPolicy {
                    max_payload_storage_bytes: large + 1,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    let task = local_task.unwrap();
    let reference = |bytes: &[u8], content_type: Option<&str>| WorkflowOutputRef {
        hash: crate::service::types::hash(bytes),
        size: bytes.len() as i64,
        content_type: content_type.map(str::to_owned),
    };
    for (bytes, accepted) in [(&b"xx"[..], false), (&b"x"[..], true), (&b"y"[..], false)] {
        let result = service
            .stage_payload(
                &worker,
                &task.id,
                &task.token,
                &RequestId::mint(),
                reference(bytes, None),
                objects.upload(bytes),
            )
            .await;
        if accepted {
            result.unwrap();
            let (descriptor, body) = service
                .read_task_payload(
                    &worker,
                    &task.id,
                    &task.token,
                    &reference(bytes, None),
                    Descriptor(&objects),
                )
                .await
                .unwrap();
            assert!(descriptor.content_type.is_none());
            assert_eq!(body, bytes);
            assert!(matches!(
                service
                    .read_task_payload(
                        &worker,
                        &task.id,
                        &task.token,
                        &reference(bytes, Some("text/plain")),
                        objects.open()
                    )
                    .await,
                Err(WorkflowServiceError::NotFound(_))
            ));
        } else {
            assert!(
                matches!(result, Err(WorkflowServiceError::ResourceExhausted(_))),
                "{result:?}"
            );
        }
    }
    // A descriptor with the same bytes but another content type owns another object.
    service
        .fixture_register(
            &local,
            leased_policy(
                3,
                AppPolicy {
                    max_payload_storage_bytes: large + 2,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    let typed = reference(b"x", Some("text/plain"));
    service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            typed.clone(),
            objects.upload(b"x"),
        )
        .await
        .unwrap();
    let (descriptor, body) = service
        .read_task_payload(&worker, &task.id, &task.token, &typed, Descriptor(&objects))
        .await
        .unwrap();
    assert_eq!(descriptor, typed);
    assert_eq!(body, b"x");
    let untyped = reference(b"x", None);
    let (descriptor, body) = service
        .read_task_payload(
            &worker,
            &task.id,
            &task.token,
            &untyped,
            Descriptor(&objects),
        )
        .await
        .unwrap();
    assert_eq!(descriptor, untyped);
    assert_eq!(body, b"x");
}
