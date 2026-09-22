use super::*;
use crate::{
    engine::StepCheckpoint,
    service::{app, journal, models, WorkerIdentity},
};
use zeroship_data_orm::{
    budgets::MAX_INSERT_MANY_BATCH,
    orm::{Entity, Operation, Output},
    sql::RowLimit,
    value,
};

#[compio::test]
async fn sqlite_compensation_failures_span_pages_without_crossing_scope() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    compensation_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_compensation_failures_span_pages_without_crossing_scope() {
    let fixture = PostgresFixture::start().await;
    compensation_contract(Rc::new(fixture.store.clone())).await;
}

#[compio::test]
async fn sqlite_continuation_keeps_accepted_parent_targets_across_pages() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    continuation_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_continuation_keeps_accepted_parent_targets_across_pages() {
    let fixture = PostgresFixture::start().await;
    continuation_contract(Rc::new(fixture.store.clone())).await;
}

async fn compensation_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, _deployments) = registered_service(store).await;
    let run_id = typed_id::new_workflow_run_id();
    let foreign_run = typed_id::new_workflow_run_id();
    let count = i32::try_from(RowLimit::default().get()).unwrap() + 1;
    let mut tx = service.begin().await.unwrap();
    for (app_id, run_id) in [(&local, &run_id), (&foreign, &foreign_run)] {
        seed_run(&mut tx, app_id, run_id, "Example").await;
        for generation in [0, 1] {
            let active = app_id == &local && generation == 0;
            let entries = (0..if active {count} else {1}).map(|ordinal| {
                let mut step = StepCheckpoint::completed_run(ordinal, format!("step-{ordinal}"), json!(ordinal));
                let pending = active && ordinal == 0;
                step.compensation_state = Some(if pending {"pending"} else {"failed"}.into());
                (step, if pending {None} else {Some(json!({"scope":app_id.as_str(), "generation":generation, "ordinal":ordinal}))})
            }).collect();
            seed_history(&tx, app_id, run_id, generation, entries).await;
        }
    }
    let now = tx.now().await.unwrap();
    tx.database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":local.as_str(), "id":run_id.clone()}),
            value!({"state":"compensating", "compensation_target":"failed", "due_at":now}),
        )
        .await
        .unwrap();
    tx.database()
        .collection(models::generations::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":local.as_str(), "run_id":run_id.clone(), "generation":0}),
            value!({"error":json!({"message":"original failure"}).to_string()}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let worker = WorkerIdentity::new("compensation-model-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run_id);
    assert_eq!(task.invocation.phase, "compensating");
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{
                "kind":"CompensationFailed", "ordinal":0, "name":"step-0",
                "error":{"scope":local.as_str(), "generation":0, "ordinal":0},
            }])),
        )
        .await
        .unwrap();
    let status = service
        .fixture_app(local.clone())
        .status(&run_id)
        .await
        .unwrap();
    assert_eq!(status.state, crate::operations::RunState::Failed);
    assert!(matches!(
        service
            .fixture_app(local.clone())
            .status(&foreign_run)
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let error = status.error.unwrap();
    assert_eq!(error["message"], json!("original failure"));
    let summary = &error["compensation"];
    assert_eq!(summary["outcome"], json!("partial"));
    assert_eq!(
        (&summary["total"], &summary["completed"], &summary["failed"]),
        (&json!(count), &json!(0), &json!(count))
    );
    let failures = summary["failures"].as_array().unwrap();
    assert_eq!(failures.len(), count as usize);
    for (failure, ordinal) in failures.iter().zip((0..count).rev()) {
        assert_eq!(
            *failure,
            json!({"ordinal":ordinal, "error":{"scope":local.as_str(), "generation":0, "ordinal":ordinal}})
        );
    }
    assert_eq!(
        service
            .fixture_app(foreign)
            .status(&foreign_run)
            .await
            .unwrap()
            .state,
        crate::operations::RunState::Paused
    );
}

async fn continuation_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, _deployments) = registered_service(store).await;
    let child = typed_id::new_workflow_run_id();
    let parent = typed_id::new_workflow_run_id();
    let other_parent = typed_id::new_workflow_run_id();
    let foreign_child = typed_id::new_workflow_run_id();
    let foreign_parent = typed_id::new_workflow_run_id();
    let foreign_other_parent = typed_id::new_workflow_run_id();
    let scopes = [
        (&local, &child, [&parent, &other_parent]),
        (
            &foreign,
            &foreign_child,
            [&foreign_parent, &foreign_other_parent],
        ),
    ];
    let count = i32::try_from(RowLimit::default().get()).unwrap() + 1;
    let mut tx = service.begin().await.unwrap();
    for (app_id, child_id, parents) in scopes {
        seed_run(&mut tx, app_id, child_id, "Child").await;
        for parent_id in parents {
            seed_run(&mut tx, app_id, parent_id, "Example").await;
            for generation in [0, 1] {
                let entries = (0..if app_id == &local && parent_id == &parent && generation == 0 {
                    count
                } else {
                    1
                })
                    .map(|ordinal| {
                        let mut step = StepCheckpoint::completed_run(
                            ordinal,
                            format!("child-{ordinal}"),
                            json!(null),
                        );
                        step.kind = "child".into();
                        step.state = "running".into();
                        step.output = None;
                        step.child_run_id = Some(child_id.clone());
                        step.child_workflow_name = Some("Child".into());
                        (step, None)
                    })
                    .collect();
                seed_history(&tx, app_id, parent_id, generation, entries).await;
            }
        }
    }
    let now = tx.now().await.unwrap();
    tx.database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":local.as_str(), "id":child.clone()}),
            value!({"state":"queued", "due_at":now}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let worker = WorkerIdentity::new("continuation-model-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, child);
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"ContinueAsNew", "input":"next"}])),
        )
        .await
        .unwrap();
    let status = service
        .fixture_app(local.clone())
        .status(&child)
        .await
        .unwrap();
    assert!(matches!(
        service
            .fixture_app(local.clone())
            .status(&foreign_child)
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert_eq!(status.state, crate::operations::RunState::ContinuedAsNew);
    assert!(status.output.is_none());
    let successor = status.continued_as_new_run_id.unwrap();
    assert_ne!(successor, child);
    let mut tx = service.begin().await.unwrap();
    for (app_id, child_id, parents) in scopes {
        app::lock_app(&mut tx, app_id).await.unwrap();
        for parent_id in parents {
            for generation in [0, 1] {
                let expected = child_id;
                let expected_count = if app_id == &local && parent_id == &parent && generation == 0
                {
                    count
                } else {
                    1
                };
                let history = journal::load(&mut tx, app_id, parent_id, generation)
                    .await
                    .unwrap();
                assert_eq!(history.len(), expected_count as usize);
                for step in history {
                    assert_eq!(step.child_run_id.as_ref(), Some(expected));
                }
                let waits = tx.database().collection(models::waits::Entity::COLLECTION).unwrap().count(
                    value!({"app_id":app_id.as_str(), "run_id":parent_id.as_str(), "generation":generation}), value!({}),
                ).await.unwrap();
                assert!(matches!(waits, Output::Count(n) if n == i64::from(expected_count)));
            }
        }
    }
    tx.commit().await.unwrap();
}

async fn seed_run(tx: &mut Transaction, app_id: &AppId, id: &str, name: &str) {
    app::lock_app(tx, app_id).await.unwrap();
    let deploy = app::active_deploy(tx, app_id).await.unwrap();
    let now = tx.now().await.unwrap();
    app::insert_root_run(
        tx,
        app_id,
        id,
        name,
        &deploy.id,
        &StartOptions::default(),
        now,
    )
    .await
    .unwrap();
    tx.database()
        .collection(models::generations::Entity::COLLECTION)
        .unwrap()
        .insert(value!({
            "id":storage_id(), "app_id":app_id.as_str(), "run_id":id, "generation":1, "deploy_id":deploy.id,
            "input":"null", "state":"queued", "started_at":now,
        }))
        .await
        .unwrap();
    tx.database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":app_id.as_str(), "id":id}),
            value!({"state":"paused", "due_at":null}),
        )
        .await
        .unwrap();
}

async fn seed_history(
    tx: &Transaction,
    app_id: &AppId,
    id: &str,
    generation: i64,
    entries: Vec<(StepCheckpoint, Option<serde_json::Value>)>,
) {
    let mut documents = Vec::new();
    for (step, error) in &entries {
        let accepted = if let Some(child) = &step.child_run_id {
            Some(
                crate::service::continuations::member(tx, app_id, child, 0)
                    .await
                    .unwrap()
                    .id,
            )
        } else {
            None
        };
        documents.push(value!({
            "id":storage_id(), "app_id":app_id.as_str(), "run_id":id, "generation":generation,
            "ordinal":i64::from(step.ordinal), "name":step.name.clone(), "occurrence":0,
            "origin_generation":generation, "kind":step.kind.clone(), "state":step.state.clone(),
            "record":journal::encode_checkpoint(step, accepted.as_deref(), None).unwrap(),
            "child_member_id":accepted,
            "compensation_attempts":i64::from(error.is_some()),
            "compensation_error":error.as_ref().map(|value|value.to_string()),
        }));
    }
    for chunk in documents.chunks(MAX_INSERT_MANY_BATCH) {
        tx.database()
            .collection(models::steps::Entity::COLLECTION)
            .unwrap()
            .execute(Operation::InsertMany {
                documents: zeroship_data_orm::Value::Array(chunk.to_vec()),
            })
            .await
            .unwrap();
    }
    let waits: Vec<_> = entries
        .iter()
        .filter(|(step, _)| step.kind == "child")
        .map(|(step, _)| {
            value!({
                "id":storage_id(), "app_id":app_id.as_str(), "run_id":id, "generation":generation,
                "ordinal":i64::from(step.ordinal), "kind":"child",
            })
        })
        .collect();
    for chunk in waits.chunks(MAX_INSERT_MANY_BATCH) {
        tx.database()
            .collection(models::waits::Entity::COLLECTION)
            .unwrap()
            .execute(Operation::InsertMany {
                documents: zeroship_data_orm::Value::Array(chunk.to_vec()),
            })
            .await
            .unwrap();
    }
}
