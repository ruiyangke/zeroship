use super::*;
use crate::operations::RestartOptions;
use crate::service::ControlIntent;

pub(super) async fn pages(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let parent = seed_runs(&service, &app, "Example", 1).await.remove(0);
    // More cascading children of one generation than a single ORM read returns.
    let children = seed_runs(&service, &app, "Child", row_limit() + 1).await;
    update_runs(
        &service,
        &app,
        &children,
        json!({"parent_id":parent, "parent_generation":0, "cascade":1, "depth":1}),
    )
    .await;
    let outside = seed_runs(&service, &app, "Child", 2).await;
    update_runs(
        &service,
        &app,
        &outside[..1],
        json!({"parent_id":parent, "parent_generation":0, "cascade":0, "depth":1}),
    )
    .await;
    update_runs(
        &service,
        &app,
        &outside[1..],
        json!({"parent_id":parent, "parent_generation":1, "cascade":1, "depth":1}),
    )
    .await;
    let terminal = children[0].clone();
    let leased = children[1].clone();
    update_runs(
        &service,
        &app,
        std::slice::from_ref(&terminal),
        json!({"state":"completed", "due_at":null}),
    )
    .await;
    update_runs(
        &service,
        &app,
        std::slice::from_ref(&leased),
        json!({"state":"running", "task_id":storage_id(), "due_at":i64::MAX}),
    )
    .await;

    let settled = cancel_idle(&scope, &parent).await;
    assert_eq!(settled.outcome, JobOutcome::Completed {});
    assert_eq!(
        scope.status(&parent).await.unwrap().state,
        RunState::Cancelled
    );
    // Settlement records one obligation and its first page, touching no child.
    let obligations = rows(&service, "propagations", json!({"app_id":app.as_str()})).await;
    assert_eq!(obligations.len(), 1);
    assert_eq!(obligations[0].text("kind").unwrap(), "cascade");
    assert_eq!(obligations[0].text("run_id").unwrap(), parent);
    assert_eq!(obligations[0].integer("generation").unwrap(), 0);
    assert_eq!(obligations[0].integer("revision").unwrap(), 1);
    assert_eq!(obligations[0].integer("finished").unwrap(), 0);
    assert_eq!(obligations[0].optional_text("cursor").unwrap(), None);
    for child in children.iter().chain(&outside) {
        assert_eq!(
            run_row(&service, &app, child)
                .await
                .text("control")
                .unwrap(),
            "none"
        );
    }
    // A repeated settlement of the same generation reuses the obligation.
    let before = snapshot(&service, &app).await;
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &app).await.unwrap();
    let now = tx.now().await.unwrap();
    crate::service::propagation::cascade(&tx, &app, &parent, 0, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(snapshot(&service, &app).await, before);

    assert_eq!(open_page(&scope).await.deployment_id(), None);
    let options = PropagationOptions::default();
    let chain = deliver_chain(&service, &scope, options).await;
    assert!(children.len() > row_limit() && chain.len() > 1);
    // Every live cascading child is reached exactly once across the pages.
    assert_eq!(affected(&chain), i64::try_from(children.len() - 1).unwrap());
    let successors: Vec<JobSpec> = chain
        .iter()
        .flat_map(|(_, _, result)| {
            serde_json::from_value::<Vec<JobSpec>>(result["successors"].clone()).unwrap()
        })
        .collect();
    let mut advanced = std::collections::BTreeSet::new();
    for job in &successors {
        match &job.operation {
            JobOperation::Advance {
                run_id, revision, ..
            } => {
                assert_eq!(revision.get(), 2);
                assert!(advanced.insert(run_id.as_str().to_owned()));
            }
            JobOperation::Propagate { revision, .. } => assert!(revision.get() > 1),
            _ => panic!("unexpected propagation successor"),
        }
    }
    // Idle children become runnable with their cancellation; the leased child
    // keeps its task and observes cancellation at renewal or completion.
    assert_eq!(advanced.len(), children.len() - 2);
    for child in &children {
        let row = run_row(&service, &app, child).await;
        if *child == terminal {
            assert_eq!(row.text("control").unwrap(), "none");
            assert_eq!(row.text("state").unwrap(), "completed");
        } else if *child == leased {
            assert_eq!(row.text("control").unwrap(), "cancel");
            assert_eq!(row.integer("due_at").unwrap(), i64::MAX);
            assert_eq!(row.integer("frontier_revision").unwrap(), 1);
            assert!(!advanced.contains(child));
        } else {
            assert_eq!(row.text("control").unwrap(), "cancel");
            assert_eq!(row.integer("frontier_revision").unwrap(), 2);
            assert!(row.optional_integer("due_at").unwrap().is_some());
            assert!(advanced.contains(child));
        }
    }
    // Non-cascading children and children of another generation are outside it.
    for child in &outside {
        let row = run_row(&service, &app, child).await;
        assert_eq!(row.text("control").unwrap(), "none");
        assert_eq!(row.integer("frontier_revision").unwrap(), 1);
    }
    let obligation = rows(&service, "propagations", json!({"app_id":app.as_str()}))
        .await
        .remove(0);
    assert_eq!(obligation.integer("finished").unwrap(), 1);
    assert_eq!(
        obligation.integer("revision").unwrap(),
        i64::try_from(chain.len() + 1).unwrap()
    );
    for (job, receipt, _) in [chain.first().unwrap(), chain.last().unwrap()] {
        assert_exact_replay(&service, &scope, job, receipt).await;
    }
}

pub(super) async fn restarted_source(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let worker = WorkerIdentity::new("restarted-source".into()).unwrap();
    let parent = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap()
        .id;
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, parent);
    service
        .complete(&worker, &task.id, &task.token, child_call("first"))
        .await
        .unwrap();
    let child = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &child.id,
            &child.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    // The waiting parent learns the result through its delivered notify page.
    assert_eq!(deliver_propagations(&scope).await.len(), 1);
    let resumed = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(resumed.invocation.run_id, parent);
    service
        .complete(
            &worker,
            &resumed.id,
            &resumed.token,
            execution(json!([{"kind":"RunFailed","error":{"message":"source failed"}}])),
        )
        .await
        .unwrap();
    assert_eq!(scope.status(&parent).await.unwrap().state, RunState::Failed);
    let delayed = open_page(&scope).await;

    // Every child of the failed generation finished, so the source restarts
    // while that generation's cascade page is still pending.
    scope
        .restart(&RequestId::mint(), &parent, RestartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.generation, 1);
    service
        .complete(&worker, &task.id, &task.token, child_call("second"))
        .await
        .unwrap();
    let successor = rows(
        &service,
        "runs",
        json!({"app_id":app.as_str(), "parent_id":parent, "parent_generation":1}),
    )
    .await;
    assert_eq!(successor.len(), 1);
    let successor = successor[0].text("id").unwrap();
    let running = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(running.invocation.run_id, successor);
    // The open obligation of generation zero does not fence generation one.
    assert_eq!(
        service
            .heartbeat(&worker, &running.id, &running.token)
            .await
            .unwrap()
            .control,
        ControlIntent::None
    );
    let before = run_row(&service, &app, &successor).await;
    let receipt = scope
        .propagation_job(&JobGrant::new(&delayed), PropagationOptions::default())
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    let results = page_results(&service, &app).await;
    let delayed_result = results
        .iter()
        .find(|result| result["propagation"]["kind"] == json!("cascade"))
        .unwrap();
    assert_eq!(delayed_result["affected"], json!(0));
    assert_eq!(run_row(&service, &app, &successor).await.0, before.0);
    let completed = service
        .complete(
            &worker,
            &running.id,
            &running.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    assert_eq!(completed.state, RunState::Completed);
}

pub(super) fn child_call(name: &str) -> crate::WorkflowExecution {
    execution(json!([{
        "kind":"Child", "ordinal":0, "name":name, "childWorkflowName":"Child",
        "options":{"cascade":true}
    }]))
}
