#![expect(
    clippy::future_not_send,
    reason = "management ordering uses compio journal fixtures"
)]

use super::atomic_application::persisted;
use super::*;
use zeroship_core::workflow_jobs::{JobId, JobOperation, JobOutcome};
use zeroship_data_orm::Value;

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            Box::pin($contract(Rc::new(
                sqlite_store(&directory.path().join("workflow.sqlite")).await,
            )))
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

case!(
    sqlite_management_requires_contiguous_explicit_revisions,
    postgres_management_requires_contiguous_explicit_revisions,
    ordered
);
case!(
    sqlite_management_rejects_damaged_receipt_history_and_head,
    postgres_management_rejects_damaged_receipt_history_and_head,
    damage
);
case!(
    sqlite_management_receipt_replays_without_policy_or_deployments,
    postgres_management_receipt_replays_without_policy_or_deployments,
    replay
);

async fn ordered(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store.clone()).await;
    let run = start(&service, &app_id).await;
    let scope = service.fixture_app(app_id.clone());
    let first = transition(&app_id, &run, 1, RunOperation::Pause);
    let next = transition(&app_id, &run, 2, RunOperation::Resume);
    let before = persisted(&service, &app_id).await;
    assert!(matches!(
        scope.management_job(&next).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert_eq!(persisted(&service, &app_id).await, before);
    let original = scope.management_job(&first).await.unwrap();
    let mut same_request = first.clone();
    same_request.delivery.job.id = JobId::mint();
    let competing = transition(&app_id, &run, 1, RunOperation::Cancel);
    let after = persisted(&service, &app_id).await;
    for changed in [&same_request, &competing] {
        assert!(matches!(
            scope.management_job(changed).await,
            Err(WorkflowServiceError::Conflict(_))
        ));
        assert_eq!(persisted(&service, &app_id).await, after);
    }
    let replica = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap();
    let other = replica.fixture_app(app_id.clone());
    let (left, right) = futures::join!(scope.management_job(&next), other.management_job(&next));
    assert_eq!(left.unwrap(), right.unwrap());
    assert_eq!(head(&service, &app_id, &run).await, (0, "queued".into()));
    assert_eq!(
        scope.management_job(&first.retry()).await.unwrap(),
        original
    );
    assert_eq!(receipt_count(&service, &next).await, 1);
    assert_eq!(
        scope.job_receipt(&first.delivery.job).await.unwrap(),
        Some(original)
    );
}

async fn patch(service: &WorkflowService, table: &str, id: &str, changes: Value) {
    let tx = service.begin().await.unwrap();
    tx.database()
        .collection(&format!("__zeroship_workflow_{table}"))
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":id}),
            patch: changes.clone(),
            many: false,
        })
        .await
        .unwrap();
    let rows = journal_rows(&tx, table, json!({"id":id})).await;
    assert_eq!(rows.len(), 1);
    for (field, expected) in changes.as_object().unwrap() {
        assert_eq!(rows[0].0.get(field), Some(expected));
    }
    tx.commit().await.unwrap();
}

async fn damage(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let run = start(&service, &app_id).await;
    let scope = service.fixture_app(app_id.clone());
    let first = started(&app_id, &run, 1);
    let second = transition(&app_id, &run, 2, RunOperation::Pause);
    let original = scope.management_job(&first).await.unwrap();
    scope.management_job(&second).await.unwrap();
    let tx = service.begin().await.unwrap();
    let stored = journal_rows(
        &tx,
        "management_receipts",
        json!({"id":first.delivery.job.id.as_str()}),
    )
    .await
    .remove(0);
    tx.commit().await.unwrap();
    damage_fields(&service, &scope, &first, &stored.0, &original).await;
    let encoded = serde_json::to_string(&JobOutcome::Completed {}).unwrap();
    patch(
        &service,
        "job_receipts",
        first.delivery.job.id.as_str(),
        value!({"outcome":encoded}),
    )
    .await;
    assert!(scope.job_receipt(&first.delivery.job).await.is_err());
    patch(
        &service,
        "job_receipts",
        first.delivery.job.id.as_str(),
        value!({"outcome":serde_json::to_string(&original.outcome).unwrap()}),
    )
    .await;
    assert_eq!(scope.management_job(&first).await.unwrap(), original);
    // The current head remains linked even when an older receipt is replayed.
    patch(
        &service,
        "management_receipts",
        second.delivery.job.id.as_str(),
        value!({"digest":"damaged-head"}),
    )
    .await;
    assert!(scope.management_job(&first).await.is_err());
    let digest = crate::service::types::digest(&second.delivery.job).unwrap();
    patch(
        &service,
        "management_receipts",
        second.delivery.job.id.as_str(),
        value!({"digest":digest}),
    )
    .await;
    let tx = service.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_management_receipts")
        .unwrap()
        .execute(Operation::Purge {
            filter: value!({"id":first.delivery.job.id.as_str()}),
            many: false,
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(scope.management_job(&first).await.is_err());
    assert!(scope.job_receipt(&first.delivery.job).await.is_err());
    let tx = service.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_management_receipts")
        .unwrap()
        .insert(stored.0)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(scope.management_job(&first).await.unwrap(), original);
}

async fn replay(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store.clone()).await;
    let run = RunId::mint();
    let first = started(&app_id, run.as_str(), 1);
    let original = service
        .fixture_app(app_id.clone())
        .management_job(&first)
        .await
        .unwrap();
    assert_eq!(
        original.outcome,
        JobOutcome::Management {
            outcome: ManagementOutcome::NotFound {}
        }
    );
    let reopened = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    let unconfigured = reopened.fixture_app(app_id.clone());
    let mut expired = first.retry();
    expired.expires = Instant::now();
    let before = persisted(&reopened, &app_id).await;
    assert_eq!(
        unconfigured.management_job(&expired).await.unwrap(),
        original
    );
    assert_eq!(
        unconfigured.job_receipt(&first.delivery.job).await.unwrap(),
        Some(original)
    );
    let fresh = started(&app_id, run.as_str(), 2);
    assert!(matches!(
        unconfigured.management_job(&fresh).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(persisted(&reopened, &app_id).await, before);
    let JobOperation::Management { revision, .. } = &expired.delivery.job.operation else {
        panic!("management");
    };
    assert_eq!(revision.get(), 1);
}

async fn damage_fields(
    service: &WorkflowService,
    scope: &crate::service::AppWorkflows,
    first: &Grant,
    stored: &Value,
    original: &crate::service::delivery::JobReceipt,
) {
    let cases = [
        ("request_id", value!(RequestId::mint().as_str())),
        ("run_id", value!(RunId::mint().as_str())),
        ("revision", value!(3)),
        ("digest", value!("damaged")),
        (
            "outcome",
            value!(serde_json::to_string(&ManagementOutcome::Denied {}).unwrap()),
        ),
    ];
    for (field, changed) in cases {
        let mut object = zeroship_data_orm::value::Map::new();
        object.insert(field.into(), changed);
        patch(
            service,
            "management_receipts",
            first.delivery.job.id.as_str(),
            Value::Object(object),
        )
        .await;
        let before = persisted(service, scope.app_id()).await;
        assert!(
            scope.management_job(first).await.is_err(),
            "damaged {field}"
        );
        assert!(
            scope.job_receipt(&first.delivery.job).await.is_err(),
            "damaged {field}"
        );
        assert_eq!(persisted(service, scope.app_id()).await, before);
        let mut restore = zeroship_data_orm::value::Map::new();
        restore.insert(field.into(), stored[field].clone());
        patch(
            service,
            "management_receipts",
            first.delivery.job.id.as_str(),
            Value::Object(restore),
        )
        .await;
        assert_eq!(scope.management_job(first).await.unwrap(), *original);
    }
}
