use super::*;
use crate::operations::RestartOptions;

pub(super) async fn pages(store: Rc<OrmStore>) {
    let (service, app, foreign, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let child = seed_runs(&service, &app, "Child", 1).await.remove(0);
    let other_child = seed_runs(&service, &app, "Child", 1).await.remove(0);
    // More distinct waiting parents than a single ORM read returns.
    let parents = seed_runs(&service, &app, "Example", row_limit() + 1).await;
    let [twice, paused, leased, cancelling, unrelated]: [String; 5] =
        seed_runs(&service, &app, "Example", 5)
            .await
            .try_into()
            .unwrap();
    let foreign_child = seed_runs(&service, &foreign, "Child", 1).await.remove(0);
    let foreign_parent = seed_runs(&service, &foreign, "Example", 1).await.remove(0);

    // The two waits of one parent sort first, so one page selects both.
    let mut identities: Vec<_> = (0..parents.len() + 6).map(|_| storage_id()).collect();
    identities.sort();
    let mut identities = identities.into_iter();
    let mut waits = Vec::new();
    for ordinal in 0..2 {
        waits.push(wait(identities.next().unwrap(), &twice, ordinal, &child));
    }
    for parent in parents.iter().chain([&paused, &leased, &cancelling]) {
        waits.push(wait(identities.next().unwrap(), parent, 0, &child));
    }
    waits.push(wait(
        identities.next().unwrap(),
        &unrelated,
        0,
        &other_child,
    ));
    for chunk in waits.chunks(100) {
        let tx = service.begin().await.unwrap();
        graph::seed_waits(&tx, &app, chunk).await;
        tx.commit().await.unwrap();
    }
    let tx = service.begin().await.unwrap();
    graph::seed_waits(
        &tx,
        &foreign,
        &[wait(storage_id(), &foreign_parent, 0, &foreign_child)],
    )
    .await;
    tx.commit().await.unwrap();
    let mut waiting: Vec<_> = parents.clone();
    waiting.extend([
        twice.clone(),
        leased.clone(),
        cancelling.clone(),
        unrelated.clone(),
    ]);
    update_runs(
        &service,
        &app,
        &waiting,
        json!({"state":"waiting", "due_at":null}),
    )
    .await;
    update_runs(
        &service,
        &app,
        std::slice::from_ref(&paused),
        json!({"state":"paused", "control":"pause", "due_at":null}),
    )
    .await;
    update_runs(
        &service,
        &app,
        std::slice::from_ref(&leased),
        json!({"state":"running", "task_id":storage_id(), "due_at":i64::MAX}),
    )
    .await;
    update_runs(
        &service,
        &app,
        std::slice::from_ref(&cancelling),
        json!({"control":"cancel"}),
    )
    .await;
    update_runs(
        &service,
        &foreign,
        std::slice::from_ref(&foreign_parent),
        json!({"state":"waiting", "due_at":null}),
    )
    .await;

    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &app).await.unwrap();
    let run = app::lock_run(&mut tx, &app, &child).await.unwrap();
    let now = tx.now().await.unwrap();
    crate::service::frontier::finish(&mut tx, &app, &run, RunState::Completed, None, None, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Completion records one notify obligation and wakes no parent inline.
    let obligations = rows(&service, "propagations", json!({"app_id":app.as_str()})).await;
    assert_eq!(obligations.len(), 1);
    assert_eq!(obligations[0].text("kind").unwrap(), "notify");
    assert_eq!(obligations[0].text("run_id").unwrap(), child);
    for parent in parents
        .iter()
        .chain([&twice, &paused, &cancelling, &unrelated])
    {
        let row = run_row(&service, &app, parent).await;
        assert_eq!(row.optional_integer("due_at").unwrap(), None);
    }

    let chain = deliver_chain(&service, &scope, PropagationOptions::default()).await;
    assert!(parents.len() > row_limit() && chain.len() > 1);
    assert!(chain
        .iter()
        .all(|(_, _, result)| result["superseded"] == json!(false)));
    // Each idle current parent is woken exactly once, including the parent
    // with two waits on the head.
    assert_eq!(affected(&chain), i64::try_from(parents.len() + 1).unwrap());
    for parent in parents.iter().chain([&twice]) {
        let row = run_row(&service, &app, parent).await;
        assert!(row.optional_integer("due_at").unwrap().is_some());
        assert_eq!(row.integer("frontier_revision").unwrap(), 2);
        assert_eq!(row.text("state").unwrap(), "waiting");
    }
    for (parent, due) in [
        (&paused, None),
        (&leased, Some(i64::MAX)),
        (&cancelling, None),
        (&unrelated, None),
    ] {
        let row = run_row(&service, &app, parent).await;
        assert_eq!(row.optional_integer("due_at").unwrap(), due);
        assert_eq!(row.integer("frontier_revision").unwrap(), 1);
    }
    let foreign_row = run_row(&service, &foreign, &foreign_parent).await;
    assert_eq!(foreign_row.optional_integer("due_at").unwrap(), None);
    assert!(
        rows(&service, "propagations", json!({"app_id":foreign.as_str()}))
            .await
            .is_empty()
    );
    for (job, receipt, _) in [chain.first().unwrap(), chain.last().unwrap()] {
        assert_exact_replay(&service, &scope, job, receipt).await;
    }
}

fn wait(id: String, run: &str, ordinal: i64, child: &str) -> graph::Wait {
    graph::Wait {
        id,
        run: run.to_owned(),
        generation: 0,
        ordinal,
        child: child.to_owned(),
    }
}

pub(super) async fn superseded(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let worker = WorkerIdentity::new("superseded-notify".into()).unwrap();
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
            execution(json!([{
                "kind":"Child", "ordinal":0, "name":"child", "childWorkflowName":"Child",
                "options":{}, "input":{}
            }])),
        )
        .await
        .unwrap();
    let child = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &child.id,
            &child.token,
            execution(json!([{"kind":"RunCompleted","output":"first"}])),
        )
        .await
        .unwrap();
    let waiting = run_row(&service, &app, &parent).await;
    assert_eq!(waiting.optional_integer("due_at").unwrap(), None);
    let delayed = open_page(&scope).await;

    // Restarting the terminal head advances it past the obligation's source.
    scope
        .restart(
            &RequestId::mint(),
            &child.invocation.run_id,
            RestartOptions::default(),
        )
        .await
        .unwrap();
    let receipt = scope
        .propagation_job(&JobGrant::new(&delayed), PropagationOptions::default())
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    let result = page_results(&service, &app).await.remove(0);
    assert_eq!(result["superseded"], json!(true));
    assert_eq!(result["affected"], json!(0));
    assert_eq!(result["successors"], json!([]));
    assert_eq!(run_row(&service, &app, &parent).await.0, waiting.0);
    assert_exact_replay(&service, &scope, &delayed, &receipt).await;

    // The restarted generation records its own obligation when it terminates.
    let rerun = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(rerun.invocation.run_id, child.invocation.run_id);
    assert_eq!(rerun.generation, 1);
    service
        .complete(
            &worker,
            &rerun.id,
            &rerun.token,
            execution(json!([{"kind":"RunCompleted","output":"second"}])),
        )
        .await
        .unwrap();
    assert_eq!(deliver_propagations(&scope).await.len(), 1);
    let resumed = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(resumed.invocation.run_id, parent);
    assert_eq!(resumed.invocation.journal[0].output, Some(json!("second")));
}
