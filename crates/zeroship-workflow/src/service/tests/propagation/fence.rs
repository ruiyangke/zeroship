use super::{job_door, *};
use crate::operations::RestartOptions;
use crate::service::{delivery::JobAcceptance, ControlIntent};

pub(super) async fn mid_propagation(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let objects = objects::Objects::new();
    let worker = job_door::Worker::new(&app).await;
    let parent = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap()
        .id;
    let claimed = worker.claim(&scope).await;
    assert_eq!(claimed.assignment().invocation.run_id, parent);
    claimed
        .finish(
            &scope,
            execution(json!([0, 1, 2].map(|ordinal| json!({
                "kind":"Child", "ordinal":ordinal, "name":format!("child-{ordinal}"),
                "childWorkflowName":"Child", "options":{"cascade":true}
            })))),
        )
        .await
        .unwrap();
    // All three children hold live tasks when their parent is cancelled.
    let mut leased = Vec::new();
    for _ in 0..3 {
        leased.push(worker.claim(&scope).await);
    }
    let [continuing, creating, running]: [_; 3] = leased.try_into().unwrap();
    cancel_idle(&scope, &parent).await;
    assert_eq!(
        scope.status(&parent).await.unwrap().state,
        RunState::Cancelled
    );
    for child in [&continuing, &creating, &running] {
        let row = run_row(&service, &app, &child.assignment().invocation.run_id).await;
        assert_eq!(row.text("control").unwrap(), "none");
    }

    // Before any page reaches it, a leased child's renewal reports cancellation.
    assert_eq!(
        running.renew(&scope).await.unwrap().control(),
        ControlIntent::Cancel
    );
    // Continuing as new mid-propagation settles the child as cancelled instead.
    let runs = rows(&service, "runs", json!({"app_id":app.as_str()}))
        .await
        .len();
    let _receipt = continuing
        .finish(&scope, execution(json!([{"kind":"ContinueAsNew"}])))
        .await
        .unwrap();
    assert_eq!(
        scope
            .status(&continuing.assignment().invocation.run_id)
            .await
            .unwrap()
            .state,
        RunState::Cancelled
    );
    assert_eq!(
        rows(&service, "runs", json!({"app_id":app.as_str()}))
            .await
            .len(),
        runs
    );
    assert_eq!(
        scope
            .status(&continuing.assignment().invocation.run_id)
            .await
            .unwrap()
            .output,
        None
    );
    // A child created mid-propagation belongs to its cancelled creator's own
    // obligation, which fences it before any page reaches it.
    let _receipt = creating
        .finish(&scope, cascade::child_call("grandchild"))
        .await
        .unwrap();
    assert_eq!(
        scope
            .status(&creating.assignment().invocation.run_id)
            .await
            .unwrap()
            .state,
        RunState::Cancelled
    );
    let grandchild = rows(
        &service,
        "runs",
        json!({"app_id":app.as_str(), "parent_id":creating.assignment().invocation.run_id}),
    )
    .await;
    assert_eq!(grandchild.len(), 1);
    let grandchild = grandchild[0].text("id").unwrap();
    let obligations = rows(
        &service,
        "propagations",
        json!({"app_id":app.as_str(), "finished":0}),
    )
    .await;
    let mut sources: Vec<_> = obligations
        .iter()
        .map(|row| row.text("run_id").unwrap())
        .collect();
    sources.sort();
    let mut cascading = vec![
        parent.clone(),
        creating.assignment().invocation.run_id.clone(),
    ];
    cascading.sort();
    assert_eq!(sources, cascading);
    // The fence acts where a frontier is delivered, not where a child is
    // created: the grandchild waits queued on its first Advance. That is the
    // one frontier no one has been handed, beside each obligation's first page.
    let advance = frontier_job(&scope, &grandchild).await;
    assert!(matches!(
        &advance.operation,
        JobOperation::Advance { generation: 0, revision, .. } if revision.get() == 1
    ));
    let (advances, propagations): (Vec<_>, Vec<_>) = undelivered(&scope)
        .await
        .into_iter()
        .partition(|job| matches!(job.operation, JobOperation::Advance { .. }));
    assert_eq!(advances, std::slice::from_ref(&advance));
    let mut first_pages: Vec<_> = propagations
        .iter()
        .map(|job| match &job.operation {
            JobOperation::Propagate {
                propagation_id,
                revision,
            } => (propagation_id.as_str().to_owned(), revision.get()),
            other => panic!("only frontiers and pages are left undelivered, got {other:?}"),
        })
        .collect();
    first_pages.sort();
    let mut opened: Vec<_> = obligations
        .iter()
        .map(|row| (row.text("id").unwrap(), 1))
        .collect();
    opened.sort();
    assert_eq!(first_pages, opened);
    // Delivering it settles the grandchild as cancelled instead of handing
    // anyone a task.
    let JobAcceptance::Settled(receipt) = scope.accept_job(&JobGrant::new(&advance)).await.unwrap()
    else {
        panic!("a fenced child settles on delivery without executing")
    };
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
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
    let row = run_row(&service, &app, &running.assignment().invocation.run_id).await;
    assert_eq!(row.text("control").unwrap(), "cancel");
    // The late result is staged first, so the run reports a result that exists
    // and the discarded value is the one a promotion would have recorded. A
    // completion carrying nothing would leave the status empty on its own.
    const LATE: &[u8] = br#""late""#;
    let late = output_reference(LATE);
    let staged = service
        .stage_payload(
            &worker.identity(),
            &running.assignment().id,
            &running.assignment().token,
            &RequestId::mint(),
            late.clone(),
            objects.upload(LATE),
        )
        .await
        .unwrap();
    assert!(
        objects.exists(&app, &staged.id),
        "the late result has to exist for its discard to mean anything"
    );
    let receipt = running
        .finish(
            &scope,
            execution(json!([{"kind":"RunCompleted","outputRef":late}])),
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    let status = scope
        .status(&running.assignment().invocation.run_id)
        .await
        .unwrap();
    assert_eq!(status.state, RunState::Cancelled);
    assert_eq!(status.output, None);
    assert!(matches!(
        scope
            .read_output(&running.assignment().invocation.run_id, objects.open())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
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

/// Committed intents that were neither published nor settled, in journal id
/// order: the work no one has been handed yet.
///
/// A job the door claims is published, and a job a case settles through a
/// fixture grant carries a receipt, so neither is listed here.
async fn undelivered(scope: &AppWorkflows) -> Vec<JobSpec> {
    let mut after = None;
    let mut undelivered = Vec::new();
    loop {
        let page = scope.pending_jobs(after.as_ref(), 100).await.unwrap();
        let Some(last) = page.last() else {
            return undelivered;
        };
        after = Some(last.id.clone());
        for job in page {
            if scope.job_receipt(&job).await.unwrap().is_none() {
                undelivered.push(job);
            }
        }
    }
}
