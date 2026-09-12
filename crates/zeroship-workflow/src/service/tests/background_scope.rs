#![expect(
    clippy::future_not_send,
    reason = "background contracts run customer storage on their compio thread"
)]

use super::*;
use crate::{
    engine::WorkflowOutputRef,
    service::{
        IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
        WorkerIdentity,
    },
};
use std::time::Instant;
use zeroship_storage::{backend::OnceChunk, LocalFs, StorageStore};

#[compio::test]
async fn sqlite_background_work_uses_only_host_assigned_apps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    background_contract(Rc::new(sqlite_store(&path).await), dir.path()).await;
}

#[compio::test]
async fn postgres_background_work_uses_only_host_assigned_apps() {
    let fixture = PostgresFixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    background_contract(Rc::new(fixture.store.clone()), dir.path()).await;
}

async fn seed_app(service: &WorkflowService, app: &AppId, worker: &WorkerIdentity) -> String {
    service
        .activate_deploy(
            app,
            &DeployRegistration {
                id: typed_id::generate("dep"),
                hash: "b".repeat(64),
                workflows: ["Example".into()].into(),
                schedules: vec![ScheduleRegistration {
                    name: "periodic".into(),
                    workflow_name: "Example".into(),
                    schedule: ScheduleTiming::Interval {
                        interval_ms: 3_600_000,
                        anchor: IntervalAnchor::Deploy,
                    },
                    input: json!(null),
                    overlap: ScheduleOverlap::default(),
                    catch_up: ScheduleCatchUp::default(),
                }],
            },
            &test_snapshot(),
        )
        .await
        .unwrap();
    service
        .for_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.app_id, app.as_str());
    let body = b"abandoned upload";
    service
        .stage_payload(
            worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            WorkflowOutputRef {
                hash: crate::service::types::hash(body),
                size: i64::try_from(body.len()).unwrap(),
                content_type: None,
            },
            Box::new(OnceChunk::new(bytes::Bytes::from_static(body))),
        )
        .await
        .unwrap()
        .id
}

// Populate a backlog larger than the discovery budget without giving the
// reopened host any authority for its apps. These rows model another host's
// journal state; none may be selected, advanced or repaired by this host.
async fn seed_unassigned_backlog(service: &WorkflowService, source: &AppId) {
    let mut tx = service.begin().await.unwrap();
    let apps = tx.table("app_state");
    let deploys = tx.table("deploys");
    let schedules = tx.table("schedules");
    let source_deploy = tx
        .query(
            &format!("SELECT id FROM {deploys} WHERE app_id=$1 AND active=1"),
            &[source.as_str().into()],
        )
        .await
        .unwrap()[0]
        .text("id")
        .unwrap();
    for _ in 0..140 {
        let app = AppId::mint();
        tx.execute(
            &format!("INSERT INTO {apps} (app_id,signal_epoch,last_polled_at) VALUES ($1,0,-1)"),
            &[app.as_str().into()],
        )
        .await
        .unwrap();
        tx.execute(&format!("INSERT INTO {deploys} (app_id,id,hash,manifest,created_at,active,state,snapshot_hash,snapshot_size,snapshot_epoch) SELECT $1,id,hash,manifest,created_at,active,state,snapshot_hash,snapshot_size,snapshot_epoch FROM {deploys} WHERE app_id=$2 AND active=1"), &[app.as_str().into(),source.as_str().into()]).await.unwrap();
        crate::service::app::insert_root_run(
            &mut tx,
            &app,
            &typed_id::new_workflow_run_id(),
            "Example",
            &source_deploy,
            &StartOptions::default(),
            0,
        )
        .await
        .unwrap();
        tx.execute(&format!("INSERT INTO {schedules} (app_id,id,name,workflow_name,deploy_id,definition,next_at,revision,anchor_at,last_checked_at) SELECT $1,id,name,workflow_name,deploy_id,definition,0,revision,anchor_at,-1 FROM {schedules} WHERE app_id=$2"), &[app.as_str().into(),source.as_str().into()]).await.unwrap();
    }
    tx.commit().await.unwrap();
}

#[expect(
    clippy::too_many_lines,
    reason = "the restart contract checks selection, isolation and expiry recovery together"
)]
async fn background_contract(store: Rc<OrmStore>, path: &Path) {
    let (service, assigned, foreign) = registered_service(store.clone()).await;
    let storage = StorageStore::from_backend(Arc::new(LocalFs::new(path.join("objects"))));
    let service = service.with_payload_storage(storage.clone()).unwrap();
    let worker = WorkerIdentity::new("customer-worker".into()).unwrap();
    let assigned_payload = seed_app(&service, &assigned, &worker).await;
    let foreign_payload = seed_app(&service, &foreign, &worker).await;
    seed_unassigned_backlog(&service, &foreign).await;
    let mut tx = service.begin().await.unwrap();
    for (table, column) in [
        ("tasks", "deadline"),
        ("runs", "due_at"),
        ("schedules", "next_at"),
        ("payloads", "expires_at"),
    ] {
        tx.execute(&format!("UPDATE {} SET {column}=0", tx.table(table)), &[])
            .await
            .unwrap();
    }
    tx.execute(
        &format!(
            "UPDATE {} SET expires_at=1 WHERE app_id=$1",
            tx.table("payloads")
        ),
        &[assigned.as_str().into()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let reopened = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap()
        .with_payload_storage(storage)
        .unwrap();
    assert!(reopened.poll(&worker).await.unwrap().is_none());
    assert_eq!(reopened.tick_schedules().await.unwrap(), 0);
    assert_eq!(reopened.collect_payloads(1).await.unwrap(), 0);
    reopened
        .register_app(&assigned, configured_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    let task = reopened.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.app_id, assigned.as_str());
    assert_eq!(reopened.tick_schedules().await.unwrap(), 1);

    reopened
        .register_app(
            &assigned,
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reopened.collect_payloads(1).await.unwrap(), 1);
    let payloads = service.payload_storage.as_ref().unwrap();
    assert!(payloads
        .get(assigned.as_str(), &assigned_payload)
        .await
        .unwrap()
        .is_none());
    assert!(payloads
        .get(foreign.as_str(), &foreign_payload)
        .await
        .unwrap()
        .is_some());

    let mut tx = service.begin().await.unwrap();
    tx.execute(
        &format!("UPDATE {} SET deadline=0 WHERE id=$1", tx.table("tasks")),
        &[task.id.clone().into()],
    )
    .await
    .unwrap();
    tx.execute(
        &format!("UPDATE {} SET due_at=0 WHERE app_id=$1", tx.table("runs")),
        &[assigned.as_str().into()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(reopened.poll(&worker).await.unwrap().is_none());
    let mut tx = service.begin().await.unwrap();
    let recovered = tx
        .query(
            &format!("SELECT state FROM {} WHERE id=$1", tx.table("tasks")),
            &[task.id.into()],
        )
        .await
        .unwrap();
    assert_eq!(recovered[0].text("state").unwrap(), "expired");
    let foreign_task = tx
        .query(
            &format!("SELECT state FROM {} WHERE app_id=$1", tx.table("tasks")),
            &[foreign.as_str().into()],
        )
        .await
        .unwrap();
    assert_eq!(foreign_task.len(), 1);
    assert_eq!(foreign_task[0].text("state").unwrap(), "leased");
    let foreign_occurrences = tx
        .query(
            &format!(
                "SELECT COUNT(*) AS total FROM {} WHERE app_id<>$1",
                tx.table("occurrences")
            ),
            &[assigned.as_str().into()],
        )
        .await
        .unwrap();
    assert_eq!(foreign_occurrences[0].integer("total").unwrap(), 0);
    let foreign_runs = tx
        .query(
            &format!(
                "SELECT COUNT(*) AS total FROM {} WHERE app_id<>$1 AND state<>'queued'",
                tx.table("runs")
            ),
            &[assigned.as_str().into()],
        )
        .await
        .unwrap();
    assert_eq!(
        foreign_runs[0].integer("total").unwrap(),
        1,
        "only the original foreign lease may be running"
    );
    tx.commit().await.unwrap();
}
