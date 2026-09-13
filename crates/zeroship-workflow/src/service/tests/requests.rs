use super::*;
use crate::{
    operations::{RestartOptions, RunOperation},
    service::{app, capability::SignalTarget, AppWorkflows, SignalAuthority, SignalTokenRequest},
};
use zeroship_core::service_assertion::{ServiceSigningKey, ServiceTrustBundle};

#[compio::test]
async fn sqlite_aged_request_receipts_survive_reopen_and_lifecycle_changes() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    lifecycle_receipts(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_aged_request_receipts_survive_reopen_and_lifecycle_changes() {
    let fixture = PostgresFixture::start().await;
    lifecycle_receipts(Rc::new(fixture.store.clone())).await;
}

#[compio::test]
async fn sqlite_aged_token_receipts_cannot_refresh_revoked_authority() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    token_receipts(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_aged_token_receipts_cannot_refresh_revoked_authority() {
    let fixture = PostgresFixture::start().await;
    token_receipts(Rc::new(fixture.store.clone())).await;
}

async fn age_receipts(scope: &AppWorkflows) {
    let mut tx = scope.service.begin().await.unwrap();
    app::lock_app(&mut tx, scope.app_id()).await.unwrap();
    assert!(journal_count(&tx, "requests", json!({"app_id":scope.app_id().as_str()})).await > 0);
    journal_update(
        &tx,
        "requests",
        json!({"app_id":scope.app_id().as_str()}),
        json!({"created_at":0}),
    )
    .await;
    tx.commit().await.unwrap();
}

async fn reopen(scope: &AppWorkflows) -> AppWorkflows {
    let service =
        WorkflowService::open(scope.service.store.clone(), scope.service.policies.clone())
            .await
            .unwrap()
            .with_deployments(scope.service.deployments.clone().unwrap());
    let service = match &scope.service.signal_authority {
        Some(authority) => service.with_signal_authority(authority.clone()),
        None => service,
    };
    service.for_app(scope.app_id().clone())
}

async fn lifecycle_receipts(store: Rc<OrmStore>) {
    let (service, local, foreign, _deployments) = registered_service(store).await;
    let scope = service.for_app(local.clone());
    let request = RequestId::mint();
    // No workflow key can hide an accidentally repeated acceptance.
    let options = StartOptions::default();
    let accepted = scope
        .start(&request, "Example", options.clone())
        .await
        .unwrap();
    let message = SignalOptions {
        signal_type: "ready".into(),
        payload: json!(true),
    };
    let signal = RequestId::mint();
    let delivered = scope
        .signal(&signal, &accepted.id, message.clone())
        .await
        .unwrap();
    let broadcast = RequestId::mint();
    let published = scope
        .broadcast(&broadcast, "updates", message.clone())
        .await
        .unwrap();
    let transition = RequestId::mint();
    let paused = scope
        .transition(&transition, &accepted.id, RunOperation::Pause)
        .await
        .unwrap();
    let restart = RequestId::mint();
    let restarted = scope
        .restart(&restart, &accepted.id, RestartOptions::default())
        .await
        .unwrap();
    scope
        .restart(&RequestId::mint(), &accepted.id, RestartOptions::default())
        .await
        .unwrap();
    age_receipts(&scope).await;
    let reopened = reopen(&scope).await;
    let before = scope.status(&accepted.id).await.unwrap();
    let (first, second) = futures::join!(
        scope.start(&request, "Example", options.clone()),
        reopened.start(&request, "Example", options.clone()),
    );
    assert_eq!(first.unwrap(), accepted);
    assert_eq!(second.unwrap(), accepted);
    assert_eq!(
        reopened
            .signal(&signal, &accepted.id, message.clone())
            .await
            .unwrap(),
        delivered
    );
    assert_eq!(
        reopened
            .broadcast(&broadcast, "updates", message.clone())
            .await
            .unwrap(),
        published
    );
    assert_eq!(
        reopened
            .transition(&transition, &accepted.id, RunOperation::Pause)
            .await
            .unwrap(),
        paused
    );
    assert_eq!(
        reopened
            .restart(&restart, &accepted.id, RestartOptions::default())
            .await
            .unwrap(),
        restarted
    );
    assert_eq!(scope.status(&accepted.id).await.unwrap(), before);

    assert!(matches!(
        reopened.start(&request, "Child", options.clone()).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert!(matches!(
        reopened
            .signal(&request, &accepted.id, message.clone())
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert!(matches!(
        reopened.broadcast(&broadcast, "another", message).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert_ne!(
        service
            .for_app(foreign)
            .start(&request, "Example", options.clone())
            .await
            .unwrap()
            .id,
        accepted.id
    );

    service
        .register_app(
            &local,
            configured_policy(
                2,
                AppPolicy {
                    admission: false,
                    ..AppPolicy::default()
                },
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        reopened
            .start(&request, "Example", options.clone())
            .await
            .unwrap(),
        accepted
    );
    assert_eq!(
        reopened.start(&RequestId::mint(), "Example", options).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(&tx, "runs", json!({"app_id":local.as_str()})).await,
        1
    );
    assert_eq!(
        journal_count(
            &tx,
            "generations",
            json!({"app_id":local.as_str(), "run_id":accepted.id})
        )
        .await,
        3
    );
    assert_eq!(
        journal_count(&tx, "signals", json!({"app_id":local.as_str()})).await,
        1
    );
    assert_eq!(
        journal_count(&tx, "broadcasts", json!({"app_id":local.as_str()})).await,
        1
    );
    let receipts = journal_rows(&tx, "requests", json!({"app_id":local.as_str()})).await;
    assert!(!receipts.is_empty());
    assert!(receipts
        .iter()
        .all(|row| row.integer("created_at").unwrap() == 0));
    tx.commit().await.unwrap();
}

async fn token_receipts(store: Rc<OrmStore>) {
    let (service, local, _, _deployments) = registered_service(store).await;
    let service = service.with_signal_authority(Arc::new(
        SignalAuthority::new(
            Arc::new(ServiceSigningKey::generate()),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
    ));
    let scope = service.for_app(local.clone());
    let target = SignalTarget::Topic {
        topic: "updates".into(),
    };
    let options = SignalTokenRequest {
        target: target.clone(),
        types: ["ready".into()].into(),
        lifetime_seconds: 60,
    };
    let request = RequestId::mint();
    let issued = scope
        .issue_signal_token(&request, options.clone())
        .await
        .unwrap();
    let message = SignalOptions {
        signal_type: "ready".into(),
        payload: json!(true),
    };
    let ingress_request = RequestId::mint();
    let accepted = service
        .ingest_signal(
            &ingress_request,
            issued.as_str(),
            &local,
            &target,
            message.clone(),
        )
        .await
        .unwrap();
    age_receipts(&scope).await;
    let reopened = reopen(&scope).await;
    assert_eq!(
        reopened
            .service
            .ingest_signal(
                &ingress_request,
                issued.as_str(),
                &local,
                &target,
                message.clone()
            )
            .await
            .unwrap(),
        accepted
    );

    let revocation = RequestId::mint();
    let revoked = scope
        .revoke_signal_tokens(&revocation, Some(target.clone()))
        .await
        .unwrap();
    let later = scope
        .revoke_signal_tokens(&RequestId::mint(), Some(target.clone()))
        .await
        .unwrap();
    assert!(later.epoch > revoked.epoch);
    age_receipts(&scope).await;
    let replayed = reopened
        .issue_signal_token(&request, options.clone())
        .await
        .unwrap();
    assert_eq!(replayed.as_str(), issued.as_str());
    assert_eq!(
        reopened
            .revoke_signal_tokens(&revocation, Some(target.clone()))
            .await
            .unwrap(),
        revoked
    );
    assert_eq!(
        reopened
            .service
            .ingest_signal(
                &ingress_request,
                replayed.as_str(),
                &local,
                &target,
                message.clone()
            )
            .await,
        Err(WorkflowServiceError::Unauthenticated)
    );
    let fresh = reopened
        .issue_signal_token(&RequestId::mint(), options)
        .await
        .unwrap();
    assert_ne!(fresh.as_str(), replayed.as_str());
    // A fresh valid capability recovers the same accepted ingress result.
    assert_eq!(
        reopened
            .service
            .ingest_signal(&ingress_request, fresh.as_str(), &local, &target, message)
            .await
            .unwrap(),
        accepted
    );
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(&tx, "broadcasts", json!({"app_id":local.as_str()})).await,
        1
    );
    tx.commit().await.unwrap();
}
