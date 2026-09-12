use super::*;
use crate::service::{
    app, models, IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleRegistration,
    ScheduleTiming,
};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, Operation, Output},
    sql::{compile::MAX_INSERT_MANY_BATCH, RowLimit},
    value,
};

#[compio::test]
async fn sqlite_schedule_reconciliation_spans_history_pages_within_app_scope() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    history_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_schedule_reconciliation_spans_history_pages_within_app_scope() {
    let fixture = PostgresFixture::start().await;
    history_contract(Rc::new(fixture.store.clone())).await;
}

#[compio::test]
async fn sqlite_schedule_revision_overflow_rolls_back_activation() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    overflow_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_schedule_revision_overflow_rolls_back_activation() {
    let fixture = PostgresFixture::start().await;
    overflow_contract(Rc::new(fixture.store.clone())).await;
}

fn registration(name: &str) -> ScheduleRegistration {
    ScheduleRegistration {
        name: name.into(),
        workflow_name: "Example".into(),
        schedule: ScheduleTiming::Interval {
            interval_ms: 3_600_000,
            anchor: IntervalAnchor::Deploy,
        },
        input: json!({"schedule":name}),
        overlap: ScheduleOverlap::default(),
        catch_up: ScheduleCatchUp::default(),
    }
}

fn deployment(schedules: Vec<ScheduleRegistration>) -> DeployRegistration {
    let id = typed_id::generate("dep");
    DeployRegistration {
        hash: crate::service::types::hash(id.as_bytes()),
        id,
        workflows: ["Example".into()].into(),
        schedules,
    }
}

async fn schedule(tx: &Transaction, app: &AppId, name: &str) -> models::ScheduleRecord {
    let rows = tx
        .database()
        .entity::<models::schedules::Entity>()
        .unwrap()
        .find::<models::ScheduleRecord>(
            models::schedules::app_id
                .eq(app.as_str())
                .unwrap()
                .and(models::schedules::name.eq(name).unwrap()),
            FindOptions {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    rows.into_iter().next().unwrap()
}

async fn history_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, deployments) = registered_service(store).await;
    let count = RowLimit::default().get() as usize + 1;
    let mut ids: Vec<_> = (0..count)
        .map(|_| typed_id::new_workflow_schedule_id())
        .collect();
    ids.sort();
    let mut tx = service.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    for app_id in [&local, &foreign] {
        app::lock_app(&mut tx, app_id).await.unwrap();
        let deploy = app::active_deploy(&mut tx, app_id).await.unwrap();
        let documents: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let item = registration(&format!("historical-{index}"));
                value!({
                    "app_id":app_id.as_str(), "id":id.clone(), "name":item.name.clone(),
                    "workflow_name":item.workflow_name.clone(), "deploy_id":deploy.id.clone(),
                    "definition":serde_json::to_string(&item).unwrap(),
                    "next_at":if index + 1 == count {None} else {Some(now + 3_600_000)},
                    "anchor_at":now, "revision":0, "last_checked_at":0,
                })
            })
            .collect();
        for chunk in documents.chunks(MAX_INSERT_MANY_BATCH) {
            tx.database()
                .collection(models::schedules::Entity::COLLECTION)
                .unwrap()
                .execute(Operation::InsertMany {
                    documents: zeroship_data_orm::Value::Array(chunk.to_vec()),
                })
                .await
                .unwrap();
        }
    }
    tx.commit().await.unwrap();
    let selected = registration(&format!("historical-{}", count - 1));
    let deploy = deployment(vec![selected.clone()]);
    deployments
        .activate(&service, &local, &deploy)
        .await
        .unwrap();
    let tx = service.begin().await.unwrap();
    let current = schedule(&tx, &local, &selected.name).await;
    assert_eq!(current.id, *ids.last().unwrap());
    assert_eq!(current.deploy_id, deploy.id);
    assert_eq!(current.revision, 1);
    assert!(current.next_at.unwrap() > now);
    assert_eq!(
        serde_json::from_str::<ScheduleRegistration>(&current.definition).unwrap(),
        selected
    );
    let schedules = tx
        .database()
        .collection(models::schedules::Entity::COLLECTION)
        .unwrap();
    for (app_id, active) in [(&local, 1), (&foreign, count as i64 - 1)] {
        assert!(
            matches!(schedules.count(value!({"app_id":app_id.as_str()}), value!({})).await.unwrap(),
            Output::Count(n) if n == count as i64)
        );
        assert!(
            matches!(schedules.count(value!({"app_id":app_id.as_str(), "next_at":{"$exists":true}}), value!({})).await.unwrap(),
            Output::Count(n) if n == active)
        );
    }
    let other = schedule(&tx, &foreign, &selected.name).await;
    assert_eq!(other.id, current.id);
    assert_ne!(other.deploy_id, current.deploy_id);
    assert_eq!(other.revision, 0);
    assert!(other.next_at.is_none());
    tx.commit().await.unwrap();
    deployments
        .activate(&service, &local, &deploy)
        .await
        .unwrap();
    let tx = service.begin().await.unwrap();
    let retry = schedule(&tx, &local, &selected.name).await;
    assert_eq!(retry.id, current.id);
    assert_eq!(retry.revision, current.revision);
    assert_eq!(retry.next_at, current.next_at);
    tx.commit().await.unwrap();
}

async fn overflow_contract(store: Rc<OrmStore>) {
    let (service, app_id, _, deployments) = registered_service(store).await;
    let original = deployment(vec![registration("billing"), registration("removed")]);
    deployments
        .activate(&service, &app_id, &original)
        .await
        .unwrap();
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &app_id).await.unwrap();
    let billing = schedule(&tx, &app_id, "billing").await;
    let removed = schedule(&tx, &app_id, "removed").await;
    tx.database()
        .collection(models::schedules::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":app_id.as_str(), "id":billing.id.clone()}),
            value!({"revision":i64::MAX}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let replacement = deployment(vec![registration("billing")]);
    let result = deployments.activate(&service, &app_id, &replacement).await;
    assert!(
        matches!(&result, Err(WorkflowServiceError::Internal(message))
        if message == "workflow schedule revision overflow"),
        "{result:?}"
    );
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &app_id).await.unwrap();
    assert_eq!(
        app::active_deploy(&mut tx, &app_id).await.unwrap().id,
        original.id
    );
    let unchanged = schedule(&tx, &app_id, "billing").await;
    assert_eq!(unchanged.revision, i64::MAX);
    assert_eq!(unchanged.deploy_id, original.id);
    assert_eq!(unchanged.next_at, billing.next_at);
    assert_eq!(unchanged.definition, billing.definition);
    let restored = schedule(&tx, &app_id, "removed").await;
    assert_eq!(restored.next_at, removed.next_at);
    assert!(restored.next_at.is_some());
    tx.commit().await.unwrap();
}
