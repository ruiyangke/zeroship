use super::*;
use crate::{
    engine::StepCheckpoint,
    operations::{RestartOptions, RunState},
    service::{app, frontier, journal, models, types::storage_id},
};
use zeroship_data_orm::{
    budgets::MAX_INSERT_MANY_BATCH,
    orm::{Entity, Operation},
    sql::RowLimit,
    value, Value,
};

#[compio::test]
async fn sqlite_child_graph_rejects_ancestors_and_current_generation_cycles() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    dependency_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_child_graph_rejects_ancestors_and_current_generation_cycles() {
    let fixture = PostgresFixture::start().await;
    dependency_contract(Rc::new(fixture.store.clone())).await;
}

#[compio::test]
async fn sqlite_completion_wakes_current_parents_across_wait_pages() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    wake_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_completion_wakes_current_parents_across_wait_pages() {
    let fixture = PostgresFixture::start().await;
    wake_contract(Rc::new(fixture.store.clone())).await;
}

#[compio::test]
async fn sqlite_restart_inspects_terminal_descendants_and_rejects_corrupt_ancestry() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    restart_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_restart_inspects_terminal_descendants_and_rejects_corrupt_ancestry() {
    let fixture = PostgresFixture::start().await;
    restart_contract(Rc::new(fixture.store.clone())).await;
}

async fn dependency_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, _deployments) = registered_service(store).await;
    let mut tx = service.begin().await.unwrap();
    let policy = app::lock_app(&mut tx, &local).await.unwrap();
    app::lock_app(&mut tx, &foreign).await.unwrap();
    let now = tx.now().await.unwrap();
    let ancestor = seed_run(&mut tx, &local, "Example", Some("ancestor")).await;
    let nested = seed_run(&mut tx, &local, "Child", None).await;
    tx.database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":local.as_str(), "id":nested.clone()}),
            value!({"parent_id":ancestor.clone(), "depth":1}),
        )
        .await
        .unwrap();
    let run = app::lock_run(&mut tx, &local, &nested).await.unwrap();
    assert!(matches!(
        journal::append(&mut tx, &local, &run, &policy,
            vec![child_checkpoint("Example", "ancestor")], now).await,
        Err(WorkflowServiceError::InvalidRequest(message))
            if message == "child workflow would wait on its ancestor"
    ));
    assert!(journal::load(&mut tx, &local, &nested, 0)
        .await
        .unwrap()
        .is_empty());

    let first = seed_run(&mut tx, &local, "Example", Some("first")).await;
    let second = seed_run(&mut tx, &local, "Child", Some("second")).await;
    let run = app::lock_run(&mut tx, &local, &first).await.unwrap();
    journal::append(
        &mut tx,
        &local,
        &run,
        &policy,
        vec![child_checkpoint("Child", "second")],
        now,
    )
    .await
    .unwrap();
    let run = app::lock_run(&mut tx, &local, &second).await.unwrap();
    assert!(matches!(
        journal::append(&mut tx, &local, &run, &policy,
            vec![child_checkpoint("Example", "first")], now).await,
        Err(WorkflowServiceError::InvalidRequest(message))
            if message == "child workflow would create a dependency cycle"
    ));
    assert!(journal::load(&mut tx, &local, &second, 0)
        .await
        .unwrap()
        .is_empty());

    let foreign_first = seed_run(&mut tx, &foreign, "Example", Some("first")).await;
    let foreign_second = seed_run(&mut tx, &foreign, "Child", Some("second")).await;
    seed_waits(
        &tx,
        &foreign,
        &[
            Wait {
                id: storage_id(),
                run: foreign_first.clone(),
                generation: 0,
                ordinal: 0,
                child: foreign_second.clone(),
            },
            Wait {
                id: storage_id(),
                run: foreign_second,
                generation: 0,
                ordinal: 0,
                child: foreign_first,
            },
        ],
    )
    .await;

    // The retained wait belongs to the previous generation. It cannot block a
    // new dependency, and another app's graph cannot enter the inspection.
    advance_generation(&mut tx, &local, &first).await;
    journal::append(
        &mut tx,
        &local,
        &run,
        &policy,
        vec![child_checkpoint("Example", "first")],
        now,
    )
    .await
    .unwrap();
    let history = journal::load(&mut tx, &local, &second, 0).await.unwrap();
    assert_eq!(history[0].child_run_id.as_deref(), Some(first.as_str()));
    tx.commit().await.unwrap();
}

async fn wake_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, _deployments) = registered_service(store).await;
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &local).await.unwrap();
    app::lock_app(&mut tx, &foreign).await.unwrap();
    let child = seed_run(&mut tx, &local, "Child", None).await;
    let other_child = seed_run(&mut tx, &local, "Child", None).await;
    let current = seed_run(&mut tx, &local, "Example", None).await;
    let last_page = seed_run(&mut tx, &local, "Example", None).await;
    let old_generation = seed_run(&mut tx, &local, "Example", None).await;
    let paused = seed_run(&mut tx, &local, "Example", None).await;
    let unrelated = seed_run(&mut tx, &local, "Example", None).await;
    let foreign_child = seed_run(&mut tx, &foreign, "Child", None).await;
    let foreign_parent = seed_run(&mut tx, &foreign, "Example", None).await;
    let page_limit = RowLimit::default().get() as usize;
    let mut identities: Vec<_> = (0..=page_limit).map(|_| storage_id()).collect();
    identities.sort();
    let mut waits: Vec<_> = identities
        .into_iter()
        .enumerate()
        .map(|(index, id)| Wait {
            id,
            run: if index == page_limit {
                last_page.clone()
            } else {
                current.clone()
            },
            generation: 0,
            ordinal: if index == page_limit { 0 } else { index as i64 },
            child: child.clone(),
        })
        .collect();
    for (run, target) in [
        (&old_generation, &child),
        (&paused, &child),
        (&unrelated, &other_child),
    ] {
        waits.push(Wait {
            id: storage_id(),
            run: run.clone(),
            generation: 0,
            ordinal: 0,
            child: target.clone(),
        });
    }
    seed_waits(&tx, &local, &waits).await;
    seed_waits(
        &tx,
        &foreign,
        &[Wait {
            id: storage_id(),
            run: foreign_parent.clone(),
            generation: 0,
            ordinal: 0,
            child: foreign_child,
        }],
    )
    .await;
    advance_generation(&mut tx, &local, &old_generation).await;
    for (app_id, run_id) in [
        (&local, &current),
        (&local, &last_page),
        (&local, &old_generation),
        (&local, &paused),
        (&local, &unrelated),
        (&foreign, &foreign_parent),
    ] {
        tx.database().collection(models::runs::Entity::COLLECTION).unwrap()
            .update(value!({"app_id":app_id.as_str(), "id":run_id.as_str()}),
                value!({"state":if run_id == &paused { "paused" } else { "waiting" }, "due_at":null}))
            .await.unwrap();
    }
    let run = app::lock_run(&mut tx, &local, &child).await.unwrap();
    let now = tx.now().await.unwrap();
    frontier::finish(&mut tx, &local, &run, RunState::Completed, None, None, now)
        .await
        .unwrap();
    for (app_id, run_id, expected) in [
        (&local, &current, Some(now)),
        (&local, &last_page, Some(now)),
        (&local, &old_generation, None),
        (&local, &paused, None),
        (&local, &unrelated, None),
        (&foreign, &foreign_parent, None),
    ] {
        let run = app::lock_run(&mut tx, app_id, run_id).await.unwrap();
        assert_eq!(
            run.optional_integer("due_at").unwrap(),
            expected,
            "{run_id}"
        );
    }
    tx.commit().await.unwrap();
}

async fn restart_contract(store: Rc<OrmStore>) {
    let (service, local, _, _deployments) = registered_service(store).await;
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &local).await.unwrap();
    let root = seed_run(&mut tx, &local, "Example", None).await;
    let middle = seed_run(&mut tx, &local, "Child", None).await;
    let leaf = seed_run(&mut tx, &local, "Child", None).await;
    let runs = tx
        .database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap();
    runs.update(
        value!({"app_id":local.as_str(), "id":middle.clone()}),
        value!({"parent_id":root.clone(), "state":"completed"}),
    )
    .await
    .unwrap();
    runs.update(
        value!({"app_id":local.as_str(), "id":leaf.clone()}),
        value!({"parent_id":middle.clone()}),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let scope = service.fixture_app(local.clone());
    let request = RequestId::mint();
    assert!(matches!(
        scope
            .restart(&request, &root, RestartOptions::default())
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &local).await.unwrap();
    let unchanged = app::lock_run(&mut tx, &local, &root).await.unwrap();
    assert_eq!(unchanged.integer("generation").unwrap(), 0);
    tx.database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":local.as_str(), "id":leaf.clone()}),
            value!({"state":"completed"}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    scope
        .restart(&request, &root, RestartOptions::default())
        .await
        .unwrap();
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &local).await.unwrap();
    tx.database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":local.as_str(), "id":root.clone()}),
            value!({"state":"completed", "parent_id":leaf.clone()}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        matches!(scope.restart(&RequestId::mint(), &root, RestartOptions::default()).await,
        Err(WorkflowServiceError::Internal(message)) if message == "workflow descendants contain a cycle")
    );
}

async fn seed_run(tx: &mut Transaction, app_id: &AppId, name: &str, key: Option<&str>) -> String {
    let id = typed_id::new_workflow_run_id();
    let deploy = app::active_deploy(tx, app_id).await.unwrap();
    let now = tx.now().await.unwrap();
    app::insert_root_run(
        tx,
        app_id,
        &id,
        name,
        &deploy.id,
        &StartOptions {
            key: key.map(str::to_owned),
            ..Default::default()
        },
        now,
    )
    .await
    .unwrap();
    id
}

async fn advance_generation(tx: &mut Transaction, app_id: &AppId, run: &str) {
    let deploy = app::active_deploy(tx, app_id).await.unwrap();
    let now = tx.now().await.unwrap();
    tx.database().collection(models::generations::Entity::COLLECTION).unwrap()
        .insert(value!({"id":storage_id(), "app_id":app_id.as_str(), "run_id":run,
            "generation":1, "deploy_id":deploy.id, "input":"null", "state":"queued", "started_at":now}))
        .await.unwrap();
    tx.database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":app_id.as_str(), "id":run}),
            value!({"generation":1}),
        )
        .await
        .unwrap();
}

fn child_checkpoint(workflow: &str, key: &str) -> StepCheckpoint {
    let execution = execution(json!([{
        "kind":"Child", "ordinal":0, "name":"join", "childWorkflowName":workflow, "options":{"key":key},
    }]));
    crate::engine::fold_outcomes(&execution.outcomes)
        .unwrap()
        .0
        .pop()
        .unwrap()
}

struct Wait {
    id: String,
    run: String,
    generation: i64,
    ordinal: i64,
    child: String,
}

async fn seed_waits(tx: &Transaction, app_id: &AppId, waits: &[Wait]) {
    assert!(!waits.is_empty());
    let steps: Vec<_> = waits.iter().map(|wait| {
        let mut step = StepCheckpoint::completed_run(wait.ordinal as i32, format!("child-{}", wait.ordinal), json!(null));
        step.kind = "child".into();
        step.state = "running".into();
        step.output = None;
        step.child_run_id = Some(wait.child.clone());
        value!({"id":storage_id(), "app_id":app_id.as_str(), "run_id":wait.run.clone(),
            "generation":wait.generation, "ordinal":wait.ordinal, "name":step.name.clone(), "occurrence":0,
            "origin_generation":wait.generation, "kind":"child", "state":"running", "record":serde_json::to_string(&step).unwrap()})
    }).collect();
    let edges: Vec<_> = waits.iter().map(|wait| value!({
        "id":wait.id.clone(), "app_id":app_id.as_str(), "run_id":wait.run.clone(),
        "generation":wait.generation, "ordinal":wait.ordinal, "kind":"child", "child_id":wait.child.clone(),
    })).collect();
    for (collection, documents) in [
        (models::steps::Entity::COLLECTION, steps),
        (models::waits::Entity::COLLECTION, edges),
    ] {
        for chunk in documents.chunks(MAX_INSERT_MANY_BATCH) {
            tx.database()
                .collection(collection)
                .unwrap()
                .execute(Operation::InsertMany {
                    documents: Value::Array(chunk.to_vec()),
                })
                .await
                .unwrap();
        }
    }
}
