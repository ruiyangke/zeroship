use super::*;
use crate::service::models;

pub(super) async fn pages(store: Rc<OrmStore>) {
    let (service, app, foreign, _deployments) = registered_service(store.clone()).await;
    let scope = service.fixture_app(app.clone());
    let worker = WorkerIdentity::new("fanout-order".into()).unwrap();
    let first_run = wait_on_topic(&service, &scope, &worker).await;
    wait_on_topic(&service, &scope, &worker).await;
    wait_on_topic(&service, &service.fixture_app(foreign.clone()), &worker).await;
    let first = broadcast(&scope, "first").await;
    let second = broadcast(&scope, "second").await;
    let first_job = job(&scope, &first.id, 1).await;
    let second_job = job(&scope, &second.id, 1).await;
    let before = snapshot(&scope).await;
    assert!(scope
        .fanout_job(&Grant::new(&second_job), FanoutOptions { page_size: 1 })
        .await
        .unwrap()
        .is_none());
    assert_eq!(snapshot(&scope).await, before);
    assert!(scope.job_receipt(&second_job).await.unwrap().is_none());
    let first_grant = Grant::new(&first_job);
    let receipt = scope
        .fanout_job(&first_grant, FanoutOptions { page_size: 1 })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});
    assert_eq!(count_signals(&scope).await, 1);
    let saved = snapshot(&scope).await;
    assert_eq!(
        scope
            .fanout_job(&first_grant.retry(), FanoutOptions { page_size: 8 })
            .await
            .unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(snapshot(&scope).await, saved);
    assert!(scope
        .fanout_job(&Grant::new(&second_job), FanoutOptions::default())
        .await
        .unwrap()
        .is_none());
    let next = job(&scope, &first.id, 2).await;
    let reopened = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap();
    let scope = reopened.fixture_app(app);
    assert_eq!(
        scope
            .fanout_job(&Grant::new(&next), FanoutOptions::default())
            .await
            .unwrap()
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(count_signals(&scope).await, 2);
    assert_eq!(
        scope
            .fanout_job(&Grant::new(&second_job), FanoutOptions::default())
            .await
            .unwrap()
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(count_signals(&scope).await, 4);
    assert_eq!(count_signals(&reopened.fixture_app(foreign)).await, 0);
    let binding = service.policies.current_binding(scope.app_id()).unwrap();
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .unwrap();
    let mut expired = first_grant.retry();
    expired.expires = Instant::now();
    assert_eq!(
        scope
            .fanout_job(&expired, FanoutOptions::default())
            .await
            .unwrap(),
        Some(receipt)
    );
    assert!(scope.job_receipt(&next).await.unwrap().is_some());
    assert_broadcast_order(&reopened, scope.app_id(), &first_run).await;
}

async fn assert_broadcast_order(service: &WorkflowService, app: &AppId, run: &str) {
    let tx = service.begin().await.unwrap();
    let signals = journal_rows(&tx, "signals", json!({"app_id":app.as_str(), "run_id":run})).await;
    assert_eq!(signals.len(), 2);
    let mut order = signals
        .into_iter()
        .map(|row| {
            (
                row.integer("delivery_sequence").unwrap(),
                row.text("payload").unwrap(),
            )
        })
        .collect::<Vec<_>>();
    order.sort();
    assert_eq!(
        order
            .iter()
            .map(|(_, payload)| payload.as_str())
            .collect::<Vec<_>>(),
        ["\"first\"", "\"second\""]
    );
    tx.commit().await.unwrap();
}

pub(super) async fn signals(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    direct_signal_order(&service, &scope, &app).await;
    // The topic predecessor gate also survives a regressing acceptance clock.
    let worker = WorkerIdentity::new("clock-regression".into()).unwrap();
    // Complete the initial fixture run before admitting the subscriber.
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    wait_on_topic(&service, &scope, &worker).await;
    let first = broadcast(&scope, "topic first").await;
    let second = broadcast(&scope, "topic second").await;
    let tx = service.begin().await.unwrap();
    tx.database()
        .entity::<models::broadcasts::Entity>()
        .unwrap()
        .update_many(
            models::broadcasts::id.eq(second.id.as_str()).unwrap(),
            models::broadcasts::created_at.set(1_i64).unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let later = job(&scope, &second.id, 1).await;
    assert!(scope
        .fanout_job(&Grant::new(&later), FanoutOptions::default())
        .await
        .unwrap()
        .is_none());
    scope
        .fanout_job(
            &Grant::new(&job(&scope, &first.id, 1).await),
            FanoutOptions::default(),
        )
        .await
        .unwrap()
        .unwrap();
    scope
        .fanout_job(&Grant::new(&later), FanoutOptions::default())
        .await
        .unwrap()
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(
        task.invocation.journal[0].output.as_ref().unwrap()["payload"],
        json!("topic first")
    );
}

async fn direct_signal_order(service: &WorkflowService, scope: &AppWorkflows, app: &AppId) {
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let mut tx = service.begin().await.unwrap();
    let policy = crate::service::app::lock_app(&mut tx, app).await.unwrap();
    let row = crate::service::app::lock_run(&mut tx, app, &run.id)
        .await
        .unwrap();
    let now = tx.now().await.unwrap();
    for (payload, timestamp) in [
        ("accepted first", now),
        ("accepted second", now - 1),
        ("same timestamp", now - 1),
    ] {
        Box::pin(crate::service::signals::deliver(
            &mut tx,
            app,
            &run.id,
            &SignalOptions {
                signal_type: "news".into(),
                payload: json!(payload),
            },
            "app",
            timestamp,
        ))
        .await
        .unwrap();
    }
    let checkpoints = (0..3)
        .map(|ordinal| {
            let mut step = crate::engine::StepCheckpoint::completed_run(
                ordinal,
                format!("wait-{ordinal}"),
                json!(null),
            );
            step.kind = "wait_signal".into();
            step.state = "running".into();
            step.output = None;
            step.signal_type = Some("news".into());
            step
        })
        .collect();
    Box::pin(crate::service::journal::append(
        &mut tx,
        app,
        &row,
        &policy,
        checkpoints,
        now,
    ))
    .await
    .unwrap();
    assert!(crate::service::journal::resolve(&mut tx, app, &row, now)
        .await
        .unwrap());
    let journal = crate::service::journal::load(&mut tx, app, &run.id, 0)
        .await
        .unwrap();
    assert_eq!(
        journal
            .iter()
            .map(|step| step.output.as_ref().unwrap()["payload"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["accepted first", "accepted second", "same timestamp"]
    );
    tx.commit().await.unwrap();
}
