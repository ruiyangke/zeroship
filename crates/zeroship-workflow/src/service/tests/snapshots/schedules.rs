#![expect(
    clippy::future_not_send,
    reason = "snapshot scheduling tests use customer storage on their compio thread"
)]

use super::*;
use crate::service::{
    IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
    WorkerIdentity,
};

fn scheduled_deployment(count: usize) -> DeployRegistration {
    let mut deploy = deployment('c');
    deploy.schedules = (0..count)
        .map(|index| ScheduleRegistration {
            name: format!("periodic-{index}"),
            workflow_name: "Example".into(),
            schedule: ScheduleTiming::Interval {
                interval_ms: 3_600_000,
                anchor: IntervalAnchor::Deploy,
            },
            input: json!(null),
            overlap: ScheduleOverlap::default(),
            catch_up: ScheduleCatchUp::default(),
        })
        .collect();
    deploy
}

#[compio::test]
async fn sqlite_schedules_wait_for_snapshot_repair_without_blocking_other_apps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    scheduled_snapshot_contract(Arc::new(SqliteStore::new(path)), dir.path()).await;
}

#[compio::test]
async fn postgres_schedules_wait_for_snapshot_repair_without_blocking_other_apps() {
    let fixture = PostgresFixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    scheduled_snapshot_contract(Arc::new(fixture.store.clone()), dir.path()).await;
}

#[expect(
    clippy::too_many_lines,
    reason = "the contract follows executable loss through schedule deferral and repair"
)]
async fn scheduled_snapshot_contract(store: Arc<dyn WorkflowStore>, path: &Path) {
    let storage = StorageStore::from_backend(Arc::new(LocalFs::new(path.join("objects"))));
    let snapshots = SnapshotStore::new(&storage, 1024 * 1024).unwrap();
    let (service, app, other) = registered_with_snapshots(store, snapshots).await;
    // An unavailable deployment's due backlog exceeds the selection budget.
    let deploy = scheduled_deployment(140);
    service
        .register_app(
            &app,
            configured_policy(
                2,
                AppPolicy {
                    max_schedules: deploy.schedules.len(),
                    ..AppPolicy::default()
                },
            ),
        )
        .await
        .unwrap();
    let snapshot = test_snapshot();
    service
        .activate_deploy(&app, &deploy, &snapshot)
        .await
        .unwrap();
    service
        .activate_deploy(&other, &scheduled_deployment(1), &snapshot)
        .await
        .unwrap();
    service
        .for_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("snapshot-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let objects =
        storage.namespace(zeroship_storage::Namespace::platform("workflow-snapshots").unwrap());
    objects.delete(app.as_str(), &deploy.id).await.unwrap();
    assert!(matches!(
        service.task_snapshot(&worker, &task.id, &task.token).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    service
        .release(&worker, &task.id, &task.token)
        .await
        .unwrap();

    let mut tx = service.begin().await.unwrap();
    let schedules = tx.table("schedules");
    let due = tx.now().await.unwrap() - 1;
    tx.execute(&format!("UPDATE {schedules} SET next_at=$1"), &[due.into()])
        .await
        .unwrap();
    tx.execute(
        &format!("UPDATE {schedules} SET last_checked_at=-1 WHERE app_id=$1"),
        &[app.as_str().into()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        service.tick_schedules().await.unwrap(),
        1,
        "unavailable code blocked another app's schedule"
    );
    assert_eq!(service.tick_schedules().await.unwrap(), 0);
    let mut tx = service.begin().await.unwrap();
    let occurrences = tx.table("occurrences");
    let deferred = tx
        .query(
            &format!("SELECT COUNT(*) AS total FROM {schedules} WHERE app_id=$1 AND next_at=$2"),
            &[app.as_str().into(), due.into()],
        )
        .await
        .unwrap();
    assert_eq!(
        usize::try_from(deferred[0].integer("total").unwrap()).unwrap(),
        deploy.schedules.len()
    );
    let admitted = tx
        .query(
            &format!("SELECT COUNT(*) AS total FROM {occurrences} WHERE app_id=$1"),
            &[app.as_str().into()],
        )
        .await
        .unwrap();
    assert_eq!(admitted[0].integer("total").unwrap(), 0);
    tx.commit().await.unwrap();

    service
        .retain_deploy(&app, &deploy, &snapshot)
        .await
        .unwrap();
    let mut fired = 0;
    loop {
        let next = service.tick_schedules().await.unwrap();
        if next == 0 {
            break;
        }
        fired += next;
        assert!(
            fired <= deploy.schedules.len(),
            "repair duplicated a scheduled occurrence"
        );
    }
    assert_eq!(fired, deploy.schedules.len());
    let mut tx = service.begin().await.unwrap();
    let resumed = tx
        .query(
            &format!("SELECT COUNT(*) AS total FROM {occurrences} WHERE app_id=$1 AND at=$2"),
            &[app.as_str().into(), due.into()],
        )
        .await
        .unwrap();
    assert_eq!(
        usize::try_from(resumed[0].integer("total").unwrap()).unwrap(),
        deploy.schedules.len()
    );
    tx.commit().await.unwrap();
}

#[compio::test]
async fn postgres_schedule_rechecks_snapshot_after_waiting_for_app_lock() {
    use std::time::Duration;
    let fixture = PostgresFixture::start().await;
    let (service, app, _) = registered_service(Arc::new(fixture.store.clone())).await;
    service
        .activate_deploy(&app, &scheduled_deployment(1), &test_snapshot())
        .await
        .unwrap();
    let mut tx = service.begin().await.unwrap();
    tx.execute(
        &format!("UPDATE {} SET next_at=0", tx.table("schedules")),
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let blocker = connect(&fixture.admin_url).await;
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker
        .query_one(
            "SELECT app_id FROM customer.__zeroship_workflow_app_state WHERE app_id=$1 FOR UPDATE",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    let ticking = compio::runtime::spawn(async move { service.tick_schedules().await });
    let observer = connect(&fixture.admin_url).await;
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = observer.query_one("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE usename='customer_worker' AND wait_event_type='Lock' AND query LIKE 'SELECT app_id FROM %')", &[]).await.unwrap().get(0);
            if waiting { break; }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("scheduler reached the customer app lock");
    blocker.execute("UPDATE customer.__zeroship_workflow_deploys SET state='unavailable' WHERE app_id=$1 AND active=1", &[&app.as_str()]).await.unwrap();
    blocker.batch_execute("COMMIT").await.unwrap();
    assert_eq!(ticking.await.unwrap().unwrap(), 0);
    let occurrences: i64 = observer
        .query_one(
            "SELECT COUNT(*) FROM customer.__zeroship_workflow_occurrences",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(occurrences, 0);
    let next: Option<i64> = observer
        .query_one(
            "SELECT next_at FROM customer.__zeroship_workflow_schedules WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(next, Some(0));
}
