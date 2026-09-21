#![expect(
    clippy::future_not_send,
    reason = "management tests own compio-local journal transactions"
)]

use super::*;
use std::collections::BTreeMap;
use zeroship_data_orm::Value;

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            let store = sqlite_store(&directory.path().join("workflow.sqlite")).await;
            Box::pin($contract(Rc::new(store))).await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

case!(
    sqlite_management_authority_loss_cannot_commit_a_refusal_or_lifecycle_change,
    postgres_management_authority_loss_cannot_commit_a_refusal_or_lifecycle_change,
    authority_loss
);
case!(
    sqlite_management_delivery_retains_original_expiry_after_refresh,
    postgres_management_delivery_retains_original_expiry_after_refresh,
    original_expiry
);
case!(
    sqlite_management_delivery_rejects_foreign_authority_and_scope,
    postgres_management_delivery_rejects_foreign_authority_and_scope,
    foreign_binding
);
case!(
    sqlite_management_cancelled_app_lock_has_no_receipt,
    postgres_management_cancelled_app_lock_has_no_receipt,
    caller_cancel
);
case!(
    sqlite_management_shorter_delivery_expires_without_a_durable_refusal,
    postgres_management_shorter_delivery_expires_without_a_durable_refusal,
    delivery_expiry
);

type Snapshot = BTreeMap<&'static str, Vec<Value>>;

pub(super) async fn persisted(service: &WorkflowService, app: &AppId) -> Snapshot {
    let tx = service.begin().await.unwrap();
    let mut state = BTreeMap::new();
    for table in [
        "runs",
        "generations",
        "job_publications",
        "outbox",
        "management_receipts",
        "job_receipts",
        "requests",
    ] {
        state.insert(
            table,
            journal_rows(&tx, table, json!({"app_id":app.as_str()}))
                .await
                .into_iter()
                .map(|row| row.0)
                .collect(),
        );
    }
    tx.commit().await.unwrap();
    state
}

async fn authority_loss(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    for refusal in [false, true] {
        let run = if refusal {
            RunId::mint().as_str().to_owned()
        } else {
            start(&service, &app_id).await
        };
        let request = started(&app_id, &run, 1);
        let scope = service.fixture_app(app_id.clone());
        let before = persisted(&service, &app_id).await;
        let mut blocker = service.begin().await.unwrap();
        app::lock_app(&mut blocker, &app_id).await.unwrap();
        let mut pending = scope.management_outcome(&request).boxed_local();
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let binding = service.policies.current_binding(&app_id).unwrap();
        binding.revoke().unwrap();
        assert!(matches!(
            compio::time::timeout(Duration::from_secs(3), pending)
                .await
                .unwrap(),
            Err(WorkflowServiceError::Unavailable(_))
        ));
        blocker.commit().await.unwrap();
        assert_eq!(persisted(&service, &app_id).await, before);
        service
            .policies
            .fixture_install(&app_id, leased_policy(1, AppPolicy::default()))
            .unwrap();
        assert_eq!(
            service
                .fixture_app(app_id.clone())
                .management_outcome(&request.retry())
                .await
                .unwrap(),
            if refusal {
                ManagementOutcome::NotFound {}
            } else {
                ManagementOutcome::Applied {
                    state: RunState::Queued,
                }
            }
        );
        assert_eq!(receipt_count(&service, &request).await, 1);
    }
}

async fn original_expiry(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let run = start(&service, &app_id).await;
    let request = started(&app_id, &run, 1);
    let scope = service.fixture_app(app_id.clone());
    let before = persisted(&service, &app_id).await;
    let mut blocker = service.begin().await.unwrap();
    app::lock_app(&mut blocker, &app_id).await.unwrap();
    let expires = Instant::now() + Duration::from_secs(1);
    service
        .policies
        .fixture_install(
            &app_id,
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), expires).unwrap(),
        )
        .unwrap();
    let mut pending = scope.management_outcome(&request).boxed_local();
    assert!(futures::poll!(pending.as_mut()).is_pending());
    service
        .policies
        .fixture_install(
            &app_id,
            PolicySnapshot::lease(
                2.try_into().unwrap(),
                AppPolicy::default(),
                expires + Duration::from_secs(30),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        compio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap(),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    scope.capture_policy().authority().unwrap().check().unwrap();
    blocker.commit().await.unwrap();
    assert_eq!(persisted(&service, &app_id).await, before);
    assert_eq!(
        scope.management_outcome(&request.retry()).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    assert_eq!(receipt_count(&service, &request).await, 1);
}

async fn foreign_binding(store: Rc<OrmStore>) {
    let (service, app_id, foreign, _deployments) = registered_service(store).await;
    let run = start(&service, &app_id).await;
    let scope = service.fixture_app(app_id.clone());
    let other = service.fixture_app(foreign.clone());
    let request = started(&app_id, &run, 1);
    let before = persisted(&service, &app_id).await;
    let foreign_before = persisted(&service, &foreign).await;
    assert!(matches!(
        other.management_outcome(&request).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(matches!(
        scope
            .clone()
            .with_authority(other.capture_policy().authority().unwrap().clone()),
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let mut substituted = request.clone();
    substituted.delivery.job.app_id = foreign.clone();
    assert!(matches!(
        scope.management_outcome(&substituted).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(persisted(&service, &app_id).await, before);
    assert_eq!(persisted(&service, &foreign).await, foreign_before);
    scope.management_outcome(&request).await.unwrap();
    assert_eq!(persisted(&service, &foreign).await, foreign_before);
}

async fn caller_cancel(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let run = start(&service, &app_id).await;
    let scope = service.fixture_app(app_id.clone());
    let request = started(&app_id, &run, 1);
    let before = persisted(&service, &app_id).await;
    let mut blocker = service.begin().await.unwrap();
    app::lock_app(&mut blocker, &app_id).await.unwrap();
    let mut pending = scope.management_outcome(&request).boxed_local();
    assert!(futures::poll!(pending.as_mut()).is_pending());
    drop(pending);
    blocker.commit().await.unwrap();
    assert_eq!(persisted(&service, &app_id).await, before);
    scope.management_outcome(&request.retry()).await.unwrap();
    assert_eq!(head(&service, &app_id, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);
}

async fn delivery_expiry(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let run = start(&service, &app_id).await;
    let scope = service.fixture_app(app_id.clone());
    let before = persisted(&service, &app_id).await;
    let mut blocker = service.begin().await.unwrap();
    app::lock_app(&mut blocker, &app_id).await.unwrap();
    let mut request = started(&app_id, &run, 1);
    request.expires = Instant::now() + Duration::from_secs(1);
    let mut pending = scope.management_outcome(&request).boxed_local();
    assert!(futures::poll!(pending.as_mut()).is_pending());
    assert!(matches!(
        compio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap(),
        Err(WorkflowServiceError::Timeout)
    ));
    scope.capture_policy().authority().unwrap().check().unwrap();
    blocker.commit().await.unwrap();
    assert_eq!(persisted(&service, &app_id).await, before);
    scope.management_outcome(&request.retry()).await.unwrap();
    assert_eq!(head(&service, &app_id, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);
}
