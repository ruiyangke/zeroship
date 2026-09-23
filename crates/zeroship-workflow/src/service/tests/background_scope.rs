#![expect(
    clippy::future_not_send,
    reason = "background contracts run customer storage on their compio thread"
)]

use super::objects::Objects;
use super::*;
use crate::{engine::WorkflowOutputRef, service::WorkerIdentity};
use std::time::Instant;

#[compio::test]
async fn sqlite_background_work_uses_only_host_assigned_apps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    background_contract(Rc::new(sqlite_store(&path).await)).await;
}

#[compio::test]
async fn postgres_background_work_uses_only_host_assigned_apps() {
    let fixture = PostgresFixture::start().await;
    background_contract(Rc::new(fixture.store.clone())).await;
}

async fn seed_app(
    deployments: &Deployments,
    service: &WorkflowService,
    app: &AppId,
    worker: &WorkerIdentity,
    objects: &Objects,
) -> String {
    deployments
        .activate(
            service,
            app,
            &DeployRegistration {
                id: typed_id::generate("dep"),
                hash: "b".repeat(64),
                workflows: ["Example".into()].into(),
                schedules: vec![],
            },
        )
        .await
        .unwrap();
    service
        .fixture_app(app.clone())
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
            objects.upload(body),
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
    let source_deploy = journal_rows(
        &tx,
        "deploys",
        json!({"app_id":source.as_str(), "active":1}),
    )
    .await
    .remove(0);
    for _ in 0..140 {
        let app = AppId::mint();
        journal_insert(&tx, "app_state", json!({"id":storage_id(), "app_id":app.as_str(), "signal_epoch":0, "last_polled_at":-1})).await.unwrap();
        let deploy_id = typed_id::generate("dep");
        let mut deploy = serde_json::to_value(&source_deploy.0).unwrap();
        deploy["id"] = json!(deploy_id);
        deploy["app_id"] = json!(app.as_str());
        journal_insert(&tx, "deploys", deploy).await.unwrap();
        crate::service::app::insert_root_run(
            &mut tx,
            &app,
            &crate::service::app::NewRun {
                id: &typed_id::new_workflow_run_id(),
                name: "Example",
                deploy: &deploy_id,
                options: &StartOptions::default(),
                input_source: None,
                max_input_bytes: AppPolicy::default().max_input_bytes,
            },
            0,
        )
        .await
        .unwrap();
        crate::service::signals::publish(
            &mut tx,
            &app,
            "updates",
            &SignalOptions {
                signal_type: "ready".into(),
                payload: json!(null),
            },
            "app",
            0,
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

#[expect(
    clippy::too_many_lines,
    reason = "the restart contract checks selection, isolation and expiry recovery together"
)]
async fn background_contract(store: Rc<OrmStore>) {
    let (service, assigned, foreign, deployments) = registered_service(store.clone()).await;
    let objects = Objects::new();
    let worker = WorkerIdentity::new("customer-worker".into()).unwrap();
    let assigned_payload = seed_app(&deployments, &service, &assigned, &worker, &objects).await;
    let foreign_payload = seed_app(&deployments, &service, &foreign, &worker, &objects).await;
    seed_unassigned_backlog(&service, &foreign).await;
    let assigned_broadcast = service
        .fixture_app(assigned.clone())
        .broadcast(
            &RequestId::mint(),
            "updates",
            SignalOptions {
                signal_type: "ready".into(),
                payload: json!(null),
            },
        )
        .await
        .unwrap();
    let assigned_job = fanout::job(
        &service.fixture_app(assigned.clone()),
        &assigned_broadcast.id,
        1,
    )
    .await;
    let tx = service.begin().await.unwrap();
    for (table, column) in [
        ("tasks", "deadline"),
        ("runs", "due_at"),
        ("payloads", "expires_at"),
    ] {
        journal_update(&tx, table, json!({}), json!({column:0})).await;
    }
    journal_update(
        &tx,
        "payloads",
        json!({"app_id":assigned.as_str()}),
        json!({"expires_at":1}),
    )
    .await;
    tx.commit().await.unwrap();

    let reopened = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    assert!(reopened.poll(&worker).await.unwrap().is_none());
    assert!(matches!(
        reopened
            .fixture_app(assigned.clone())
            .fanout_job(
                &fanout::Grant::new(&assigned_job),
                crate::service::fanout::FanoutOptions::default()
            )
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(reopened.collect_payloads(1, &objects).await.unwrap(), 0);
    reopened
        .fixture_register(&assigned, leased_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    let task = reopened.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.app_id, assigned.as_str());
    assert_eq!(
        fanout::deliver_topic_page(&reopened.fixture_app(assigned.clone())).await,
        0
    );
    let tx = service.begin().await.unwrap();
    let completed = journal_rows(&tx, "broadcasts", json!({"finished":1})).await;
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].text("app_id").unwrap(), assigned.as_str());
    assert_eq!(completed[0].text("id").unwrap(), assigned_broadcast.id);
    let pending = journal_count(
        &tx,
        "broadcasts",
        json!({"app_id":{"$ne":assigned.as_str()}, "finished":0}),
    )
    .await;
    assert!(pending > 128);
    tx.commit().await.unwrap();

    reopened
        .policies
        .fixture_install(
            &assigned,
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(reopened.collect_payloads(1, &objects).await.unwrap(), 1);
    assert!(!objects.exists(&assigned, &assigned_payload));
    assert!(objects.exists(&foreign, &foreign_payload));

    let tx = service.begin().await.unwrap();
    journal_update(&tx, "tasks", json!({"id":task.id}), json!({"deadline":0})).await;
    journal_update(
        &tx,
        "runs",
        json!({"app_id":assigned.as_str()}),
        json!({"due_at":0}),
    )
    .await;
    tx.commit().await.unwrap();
    assert!(reopened.poll(&worker).await.unwrap().is_none());
    let tx = service.begin().await.unwrap();
    let recovered = journal_rows(&tx, "tasks", json!({"id":task.id})).await;
    assert_eq!(recovered[0].text("state").unwrap(), "expired");
    let foreign_task = journal_rows(&tx, "tasks", json!({"app_id":foreign.as_str()})).await;
    assert_eq!(foreign_task.len(), 1);
    assert_eq!(foreign_task[0].text("state").unwrap(), "leased");
    assert_eq!(
        journal_count(
            &tx,
            "runs",
            json!({"app_id":{"$ne":assigned.as_str()}, "state":{"$ne":"queued"}})
        )
        .await,
        1,
        "only the original foreign lease may be running"
    );
    tx.commit().await.unwrap();
}
