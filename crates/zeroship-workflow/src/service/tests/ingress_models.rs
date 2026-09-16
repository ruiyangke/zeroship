use super::*;
use crate::service::{app, capability::SignalTarget, models, SignalAuthority, SignalTokenRequest};
use zeroship_core::service_assertion::{ServiceSigningKey, ServiceTrustBundle};
use zeroship_data_orm::{
    orm::{Entity, Output},
    value,
};

#[compio::test]
async fn sqlite_signal_revocation_preserves_foreign_and_other_target_authority() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    revocation_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_signal_revocation_preserves_foreign_and_other_target_authority() {
    let fixture = PostgresFixture::start().await;
    revocation_contract(Rc::new(fixture.store.clone())).await;
}

async fn revocation_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, _deployments) = registered_service(store).await;
    let service = service.with_signal_authority(Arc::new(
        SignalAuthority::new(
            Arc::new(ServiceSigningKey::generate()),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
    ));
    let run_id = typed_id::new_workflow_run_id();
    let foreign_id = typed_id::new_workflow_run_id();
    let mut tx = service.begin().await.unwrap();
    for (app_id, run_id) in [(&local, &run_id), (&foreign, &foreign_id)] {
        app::lock_app(&mut tx, app_id).await.unwrap();
        let deploy = app::active_deploy(&mut tx, app_id).await.unwrap();
        let now = tx.now().await.unwrap();
        app::insert_root_run(
            &mut tx,
            app_id,
            run_id,
            "Example",
            &deploy.id,
            &StartOptions::default(),
            now,
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let run = SignalTarget::Run {
        run_id: run_id.clone(),
    };
    let foreign_run = SignalTarget::Run { run_id: foreign_id };
    let topic = SignalTarget::Topic {
        topic: "updates".into(),
    };
    let local_scope = service.fixture_app(local.clone());
    let message = SignalOptions {
        signal_type: "ready".into(),
        payload: json!("accepted"),
    };
    for revoked_target in [Some(run.clone()), Some(topic.clone()), None] {
        let mut issued = Vec::new();
        for (app_id, run_target) in [(&local, &run), (&foreign, &foreign_run)] {
            for target in [run_target, &topic] {
                let options = SignalTokenRequest {
                    target: target.clone(),
                    types: ["ready".into()].into(),
                    lifetime_seconds: 60,
                };
                let token = service
                    .fixture_app(app_id.clone())
                    .issue_signal_token(&RequestId::mint(), options.clone())
                    .await
                    .unwrap();
                issued.push((app_id, target, options, token));
            }
        }
        let request = RequestId::mint();
        let revoked = local_scope
            .revoke_signal_tokens(&request, revoked_target.clone())
            .await
            .unwrap();
        assert_eq!(revoked.epoch, 1);
        assert_eq!(
            local_scope
                .revoke_signal_tokens(&request, revoked_target.clone())
                .await
                .unwrap(),
            revoked
        );
        for (app_id, target, options, token) in issued {
            let other_app = if app_id == &local { &foreign } else { &local };
            assert!(matches!(
                service
                    .fixture_app((other_app).clone())
                    .ingest_signal(&RequestId::mint(), token.as_str(), target, message.clone())
                    .await,
                Err(WorkflowServiceError::NotFound(_)),
            ));
            let denied = app_id == &local
                && revoked_target
                    .as_ref()
                    .is_none_or(|revoked| revoked == target);
            let result = service
                .fixture_app((app_id).clone())
                .ingest_signal(&RequestId::mint(), token.as_str(), target, message.clone())
                .await;
            if denied {
                assert_eq!(result, Err(WorkflowServiceError::Unauthenticated));
                let replacement = local_scope
                    .issue_signal_token(&RequestId::mint(), options)
                    .await
                    .unwrap();
                service
                    .fixture_app((app_id).clone())
                    .ingest_signal(
                        &RequestId::mint(),
                        replacement.as_str(),
                        target,
                        message.clone(),
                    )
                    .await
                    .unwrap();
                // Issuing another token must not reset the persisted revocation epoch.
                assert_eq!(
                    service
                        .fixture_app((app_id).clone())
                        .ingest_signal(&RequestId::mint(), token.as_str(), target, message.clone())
                        .await,
                    Err(WorkflowServiceError::Unauthenticated)
                );
            } else {
                result.unwrap();
            }
        }
    }

    for target in [Some(run), Some(topic), None] {
        let mut tx = service.begin().await.unwrap();
        app::lock_app(&mut tx, &local).await.unwrap();
        let (table, filter) = match &target {
            Some(SignalTarget::Run { run_id }) => (
                models::runs::Entity::COLLECTION,
                value!({"app_id":local.as_str(), "id":run_id.clone()}),
            ),
            Some(SignalTarget::Topic { topic }) => (
                models::topics::Entity::COLLECTION,
                value!({"app_id":local.as_str(), "topic":topic.clone()}),
            ),
            None => (
                models::app_state::Entity::COLLECTION,
                value!({"app_id":local.as_str()}),
            ),
        };
        tx.database()
            .collection(table)
            .unwrap()
            .update(filter, value!({"signal_epoch":i64::MAX}))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let request = RequestId::mint();
        assert!(matches!(
            local_scope.revoke_signal_tokens(&request, target).await,
            Err(WorkflowServiceError::ResourceExhausted(_))
        ));
        let tx = service.begin().await.unwrap();
        let receipts = tx
            .database()
            .collection(models::requests::Entity::COLLECTION)
            .unwrap()
            .count(
                value!({"app_id":local.as_str(), "request_id":request.as_str()}),
                value!({}),
            )
            .await
            .unwrap();
        assert!(matches!(receipts, Output::Count(0)));
        tx.commit().await.unwrap();
    }
}
