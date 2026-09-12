use super::*;
use crate::service::{
    app, capability::CapabilityToken, capability::SignalTarget, SignalAuthority, SignalTokenRequest,
};
use futures::{stream::FuturesUnordered, StreamExt};
use std::time::Duration;
use zeroship_core::service_assertion::{ServiceSigningKey, ServiceTrustBundle};

#[compio::test]
async fn sqlite_independent_hosts_initialize_topics_without_resetting_revocation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    let first = sqlite_store(&path).await;
    let second = sqlite_store(&path).await;
    initialization_contract(first, second).await;
}

#[compio::test]
async fn postgres_independent_hosts_initialize_topics_without_resetting_revocation() {
    let fixture = PostgresFixture::start().await;
    let second = orm_store(
        &fixture
            .admin_url
            .replacen("postgres@", "customer_worker@", 1),
        super::super::store::SchemaName::new("customer").unwrap(),
    )
    .await;
    initialization_contract(fixture.store.clone(), second).await;
}

async fn initialization_contract(first: OrmStore, second: OrmStore) {
    let policies = Arc::new(HostPolicies::default());
    let authority = Arc::new(
        SignalAuthority::new(
            Arc::new(ServiceSigningKey::generate()),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
    );
    let first = WorkflowService::open(Rc::new(first), policies.clone())
        .await
        .unwrap()
        .with_signal_authority(authority.clone());
    let second = WorkflowService::open(Rc::new(second), policies)
        .await
        .unwrap()
        .with_signal_authority(authority);
    let local = AppId::mint();
    let foreign = AppId::mint();
    for app_id in [&local, &foreign] {
        first
            .register_app(app_id, configured_policy(1, AppPolicy::default()))
            .await
            .unwrap();
    }
    let target = SignalTarget::Topic {
        topic: "updates".into(),
    };
    let original = issue_concurrently(&first, &second, &local, &target).await;
    let foreign_token = second
        .for_app(foreign.clone())
        .issue_signal_token(&RequestId::mint(), token_options(&target))
        .await
        .unwrap();
    let message = SignalOptions {
        signal_type: "ready".into(),
        payload: json!({"ready": true}),
    };
    for token in &original {
        second
            .ingest_signal(
                &RequestId::mint(),
                token.as_str(),
                &local,
                &target,
                message.clone(),
            )
            .await
            .unwrap();
    }
    let revoked = first
        .for_app(local.clone())
        .revoke_signal_tokens(&RequestId::mint(), Some(target.clone()))
        .await
        .unwrap();
    assert_eq!(revoked.epoch, 1);
    let replacements = issue_concurrently(&first, &second, &local, &target).await;
    for token in &original {
        assert_eq!(
            first
                .ingest_signal(
                    &RequestId::mint(),
                    token.as_str(),
                    &local,
                    &target,
                    message.clone(),
                )
                .await,
            Err(WorkflowServiceError::Unauthenticated)
        );
    }
    for (app_id, token) in replacements
        .iter()
        .map(|token| (&local, token))
        .chain(std::iter::once((&foreign, &foreign_token)))
    {
        second
            .ingest_signal(
                &RequestId::mint(),
                token.as_str(),
                app_id,
                &target,
                message.clone(),
            )
            .await
            .unwrap();
    }

    first
        .for_app(local.clone())
        .revoke_signal_tokens(&RequestId::mint(), None)
        .await
        .unwrap();
    let (a, b) = futures::join!(
        first.register_app(&local, configured_policy(1, AppPolicy::default())),
        second.register_app(&local, configured_policy(1, AppPolicy::default())),
    );
    a.unwrap();
    b.unwrap();
    for token in replacements {
        assert_eq!(
            second
                .ingest_signal(
                    &RequestId::mint(),
                    token.as_str(),
                    &local,
                    &target,
                    message.clone()
                )
                .await,
            Err(WorkflowServiceError::Unauthenticated)
        );
    }
    let replacement = first
        .for_app(local.clone())
        .issue_signal_token(&RequestId::mint(), token_options(&target))
        .await
        .unwrap();
    second
        .ingest_signal(
            &RequestId::mint(),
            replacement.as_str(),
            &local,
            &target,
            message,
        )
        .await
        .unwrap();
}

async fn issue_concurrently(
    first: &WorkflowService,
    second: &WorkflowService,
    app_id: &AppId,
    target: &SignalTarget,
) -> Vec<CapabilityToken> {
    let mut blocker = first.begin().await.unwrap();
    app::lock_app(&mut blocker, app_id).await.unwrap();
    let pending = [first, second]
        .into_iter()
        .map(|service| async move {
            service
                .for_app(app_id.clone())
                .issue_signal_token(&RequestId::mint(), token_options(target))
                .await
                .unwrap()
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>();
    futures::pin_mut!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    blocker.commit().await.unwrap();
    compio::time::timeout(Duration::from_secs(10), pending)
        .await
        .expect("topic issuance must complete after the app lock is released")
}

fn token_options(target: &SignalTarget) -> SignalTokenRequest {
    SignalTokenRequest {
        target: target.clone(),
        types: ["ready".into()].into(),
        lifetime_seconds: 60,
    }
}
