use super::*;
use crate::operations::RestartOptions;
use crate::service::{delivery::JobAcceptance, ControlIntent};

pub(super) async fn mid_propagation(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let worker = WorkerIdentity::new("fenced-children".into()).unwrap();
    let parent = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap()
        .id;
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, parent);
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([0, 1, 2].map(|ordinal| json!({
                "kind":"Child", "ordinal":ordinal, "name":format!("child-{ordinal}"),
                "childWorkflowName":"Child", "options":{"cascade":true}, "input":{}
            })))),
        )
        .await
        .unwrap();
    // All three children hold live tasks when their parent is cancelled.
    let mut leased = Vec::new();
    for _ in 0..3 {
        leased.push(service.poll(&worker).await.unwrap().unwrap());
    }
    let [continuing, creating, running]: [_; 3] = leased.try_into().unwrap();
    cancel_idle(&scope, &parent).await;
    assert_eq!(
        scope.status(&parent).await.unwrap().state,
        RunState::Cancelled
    );
    for child in [&continuing, &creating, &running] {
        let row = run_row(&service, &app, &child.invocation.run_id).await;
        assert_eq!(row.text("control").unwrap(), "none");
    }

    // Before any page reaches it, a leased child's renewal reports cancellation.
    assert_eq!(
        service
            .heartbeat(&worker, &running.id, &running.token)
            .await
            .unwrap()
            .control,
        ControlIntent::Cancel
    );
    // Continuing as new mid-propagation settles the child as cancelled instead.
    let runs = rows(&service, "runs", json!({"app_id":app.as_str()}))
        .await
        .len();
    let receipt = service
        .complete(
            &worker,
            &continuing.id,
            &continuing.token,
            execution(json!([{"kind":"ContinueAsNew","input":"escape"}])),
        )
        .await
        .unwrap();
    assert_eq!(receipt.state, RunState::Cancelled);
    assert_eq!(
        rows(&service, "runs", json!({"app_id":app.as_str()}))
            .await
            .len(),
        runs
    );
    assert_eq!(
        scope
            .status(&continuing.invocation.run_id)
            .await
            .unwrap()
            .output,
        None
    );
    // A child created mid-propagation belongs to its cancelled creator's own
    // obligation, which fences it before any page reaches it.
    let receipt = service
        .complete(
            &worker,
            &creating.id,
            &creating.token,
            cascade::child_call("grandchild"),
        )
        .await
        .unwrap();
    assert_eq!(receipt.state, RunState::Cancelled);
    let grandchild = rows(
        &service,
        "runs",
        json!({"app_id":app.as_str(), "parent_id":creating.invocation.run_id}),
    )
    .await;
    assert_eq!(grandchild.len(), 1);
    let grandchild = grandchild[0].text("id").unwrap();
    assert_eq!(
        rows(
            &service,
            "propagations",
            json!({"app_id":app.as_str(), "finished":0})
        )
        .await
        .len(),
        2
    );
    assert!(service.poll(&worker).await.unwrap().is_none());
    assert_eq!(
        scope.status(&grandchild).await.unwrap().state,
        RunState::Cancelled
    );

    // Pages then record cancellation on the leased child and pass over the
    // children that already finished.
    let pages = deliver_propagations_with(&scope, PropagationOptions { page_size: 1 }).await;
    assert!(pages.len() > 2);
    assert_eq!(
        rows(
            &service,
            "propagations",
            json!({"app_id":app.as_str(), "finished":0})
        )
        .await
        .len(),
        0
    );
    let row = run_row(&service, &app, &running.invocation.run_id).await;
    assert_eq!(row.text("control").unwrap(), "cancel");
    let receipt = service
        .complete(
            &worker,
            &running.id,
            &running.token,
            execution(json!([{"kind":"RunCompleted","output":"late"}])),
        )
        .await
        .unwrap();
    assert_eq!(receipt.state, RunState::Cancelled);
    assert_eq!(
        scope
            .status(&running.invocation.run_id)
            .await
            .unwrap()
            .output,
        None
    );
}

pub(super) async fn restart(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let parent = seed_runs(&service, &app, "Example", 1).await.remove(0);
    let children = seed_runs(&service, &app, "Child", 2).await;
    update_runs(
        &service,
        &app,
        &children,
        json!({"parent_id":parent, "parent_generation":0, "cascade":1, "depth":1}),
    )
    .await;
    cancel_idle(&scope, &parent).await;
    // The first child settles on its own while the cascade still propagates.
    cancel_idle(&scope, &children[0]).await;
    assert_eq!(
        scope.status(&children[0]).await.unwrap().state,
        RunState::Cancelled
    );
    let before = snapshot(&service, &app).await;
    assert!(matches!(
        scope
            .restart(&RequestId::mint(), &children[0], RestartOptions::default())
            .await,
        Err(WorkflowServiceError::Conflict(message)) if message.contains("propagating")
    ));
    assert_eq!(snapshot(&service, &app).await, before);

    let pages = deliver_propagations(&scope).await;
    assert_eq!(pages.len(), 1);
    let page = rows(
        &service,
        "propagation_pages",
        json!({"app_id":app.as_str()}),
    )
    .await
    .remove(0);
    let page_job = scope
        .pending_jobs(None, 500)
        .await
        .unwrap()
        .into_iter()
        .find(|job| job.id.as_str() == page.text("id").unwrap())
        .unwrap();
    assert_eq!(
        run_row(&service, &app, &children[1])
            .await
            .text("control")
            .unwrap(),
        "cancel"
    );
    // After the obligation finishes, restart is an explicit override.
    let restarted = scope
        .restart(&RequestId::mint(), &children[0], RestartOptions::default())
        .await
        .unwrap();
    assert_eq!(restarted.state, RunState::Queued);
    // Replaying the committed page cannot reach the restarted generation.
    assert_exact_replay(&service, &scope, &page_job, &pages[0]).await;
    let row = run_row(&service, &app, &children[0]).await;
    assert_eq!(row.text("control").unwrap(), "none");
    assert_eq!(row.integer("generation").unwrap(), 1);
    let advance = frontier_job(&scope, &children[0]).await;
    assert!(matches!(
        &advance.operation,
        JobOperation::Advance { generation: 1, .. }
    ));
    assert!(matches!(
        scope.accept_job(&JobGrant::new(&advance)).await.unwrap(),
        JobAcceptance::Execute(_)
    ));
}

pub(super) async fn delivered_renewal(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let parent = seed_runs(&service, &app, "Example", 1).await.remove(0);
    let child = seed_runs(&service, &app, "Child", 1).await.remove(0);
    update_runs(
        &service,
        &app,
        std::slice::from_ref(&child),
        json!({"parent_id":parent, "parent_generation":0, "cascade":1, "depth":1}),
    )
    .await;
    // The child holds a delivered task when its parent's cascade begins.
    let grant = JobGrant::new(&frontier_job(&scope, &child).await);
    let JobAcceptance::Execute(mut task) = scope.accept_job(&grant).await.unwrap() else {
        panic!("the idle child is delivered for execution")
    };
    let control = scope.heartbeat_job(&task, &grant).await.unwrap().control();
    assert_eq!(control, ControlIntent::None);
    cancel_idle(&scope, &parent).await;
    assert_eq!(
        run_row(&service, &app, &child)
            .await
            .text("control")
            .unwrap(),
        "none"
    );
    // Renewal reports the fence before any page records the cancellation.
    let renewal = scope.heartbeat_job(&task, &grant).await.unwrap();
    assert_eq!(renewal.control(), ControlIntent::Cancel);
    task.renew(renewal);
    deliver_propagations(&scope).await;
    let control = scope.heartbeat_job(&task, &grant).await.unwrap().control();
    assert_eq!(control, ControlIntent::Cancel);
    assert_eq!(
        run_row(&service, &app, &child)
            .await
            .text("control")
            .unwrap(),
        "cancel"
    );
}
