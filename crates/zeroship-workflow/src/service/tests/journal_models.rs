use super::*;
use crate::{
    engine::StepCheckpoint,
    service::{app, frontier, journal, models},
};
use zeroship_data_orm::{
    budgets::MAX_INSERT_MANY_BATCH,
    orm::{Entity, FindOptions, FromRow, Operation},
    sql::RowLimit,
    value, Value,
};

#[derive(FromRow)]
#[orm(entity = models::generations)]
struct GenerationLifecycle {
    state: String,
    terminal_at: Option<i64>,
}

#[compio::test]
async fn sqlite_model_journal_reads_preserve_scope_and_complete_history() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    read_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_model_journal_reads_preserve_scope_and_complete_history() {
    let fixture = PostgresFixture::start().await;
    read_contract(Rc::new(fixture.store.clone())).await;
}

async fn read_contract(store: Rc<OrmStore>) {
    let (service, first_app, second_app, _deployments) = registered_service(store).await;
    let run_id = typed_id::new_workflow_run_id();
    let foreign_run = typed_id::new_workflow_run_id();
    let other_run = typed_id::new_workflow_run_id();
    let scopes = [
        (&first_app, run_id.as_str()),
        (&second_app, foreign_run.as_str()),
        (&first_app, other_run.as_str()),
    ];
    let count = i32::try_from(RowLimit::default().get()).unwrap() + 1;
    let mut tx = service.begin().await.unwrap();
    for app_id in [&first_app, &second_app] {
        app::lock_app(&mut tx, app_id).await.unwrap();
    }
    let now = tx.now().await.unwrap();
    for (scope, (app_id, run_id)) in scopes.iter().enumerate() {
        let deploy = app::active_deploy(&mut tx, app_id).await.unwrap();
        app::insert_root_run(
            &mut tx,
            app_id,
            run_id,
            "Example",
            &deploy.id,
            &StartOptions {
                input: json!({"scope":scope, "generation":0}),
                ..Default::default()
            },
            now,
        )
        .await
        .unwrap();
        let generations = tx
            .database()
            .collection(models::generations::Entity::COLLECTION)
            .unwrap();
        generations
            .update(
                value!({"app_id":app_id.as_str(), "run_id":*run_id, "generation":0}),
                value!({"state":"completed", "terminal_at":now,
                "output":json!({"scope":scope, "generation":0}).to_string()}),
            )
            .await
            .unwrap();
        generations
            .insert(value!({
                "id":storage_id(), "app_id":app_id.as_str(), "run_id":*run_id, "generation":1, "deploy_id":deploy.id,
                "input":json!({"scope":scope, "generation":1}).to_string(),
                "state":"completed", "started_at":now, "terminal_at":now,
                "output":json!({"scope":scope, "generation":1}).to_string(),
            }))
            .await
            .unwrap();

        for generation in [0, 1] {
            let checkpoints: Vec<_> = if scope == 0 && generation == 1 {
                (0..count)
                    .rev()
                    .map(|ordinal| {
                        StepCheckpoint::completed_run(
                            ordinal,
                            format!("step-{ordinal}"),
                            json!(ordinal),
                        )
                    })
                    .collect()
            } else {
                vec![StepCheckpoint::completed_run(
                    count - 1,
                    "neighbor",
                    json!({"scope":scope, "generation":generation}),
                )]
            };
            let documents: Vec<_> = checkpoints.iter().map(|step| value!({
                "id":storage_id(), "app_id":app_id.as_str(), "run_id":*run_id, "generation":generation,
                "ordinal":i64::from(step.ordinal), "name":step.name.clone(), "occurrence":0,
                "origin_generation":generation, "kind":step.kind.clone(), "state":step.state.clone(),
                "record":serde_json::to_string(step).unwrap(),
            })).collect();
            for chunk in documents.chunks(MAX_INSERT_MANY_BATCH) {
                tx.database()
                    .collection(models::steps::Entity::COLLECTION)
                    .unwrap()
                    .execute(Operation::InsertMany {
                        documents: Value::Array(chunk.to_vec()),
                    })
                    .await
                    .unwrap();
            }
        }
    }

    let history = journal::load(&mut tx, &first_app, &run_id, 1)
        .await
        .unwrap();
    assert_eq!(history.len(), count as usize);
    for (ordinal, step) in history.iter().enumerate() {
        assert_eq!(step.ordinal, ordinal as i32);
        assert_eq!(step.output, Some(json!(ordinal)));
    }
    let mut changed = history.last().unwrap().clone();
    changed.output = Some(json!("updated"));
    journal::update(&mut tx, &first_app, &run_id, 1, &changed)
        .await
        .unwrap();
    let history = journal::load(&mut tx, &first_app, &run_id, 1)
        .await
        .unwrap();
    assert_eq!(history.len(), count as usize);
    assert_eq!(history.last().unwrap().output, changed.output);
    assert!(journal::load(&mut tx, &first_app, &foreign_run, 1)
        .await
        .unwrap()
        .is_empty());
    assert!(journal::load(&mut tx, &second_app, &run_id, 1)
        .await
        .unwrap()
        .is_empty());
    for (scope, (app_id, run_id)) in scopes.iter().enumerate() {
        for generation in [0, 1] {
            if scope == 0 && generation == 1 {
                continue;
            }
            let history = journal::load(&mut tx, app_id, run_id, generation)
                .await
                .unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(
                history[0].output,
                Some(json!({"scope":scope, "generation":generation}))
            );
        }
    }
    tx.commit().await.unwrap();

    // Move heads across generations while neighboring app histories retain the
    // same workflow, step names and ordinals.
    for generation in [0, 1] {
        let mut tx = service.begin().await.unwrap();
        for (app_id, run_id) in scopes {
            app::lock_app(&mut tx, app_id).await.unwrap();
            tx.database()
                .collection(models::runs::Entity::COLLECTION)
                .unwrap()
                .update(
                    value!({"app_id":app_id.as_str(), "id":run_id}),
                    value!({"generation":generation, "state":"completed"}),
                )
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        for (scope, (app_id, run_id)) in scopes.iter().enumerate() {
            let status = service
                .for_app((*app_id).clone())
                .status(run_id)
                .await
                .unwrap();
            assert_eq!(status.state, crate::operations::RunState::Completed);
            assert_eq!(
                status.output,
                Some(json!({"scope":scope, "generation":generation}))
            );
            let mut tx = service.begin().await.unwrap();
            app::lock_app(&mut tx, app_id).await.unwrap();
            let run = app::lock_run(&mut tx, app_id, run_id).await.unwrap();
            let invocation = frontier::invocation(&mut tx, app_id, &run).await.unwrap();
            assert_eq!(invocation.app_id, app_id.as_str());
            assert_eq!(invocation.run_id, *run_id);
            assert_eq!(
                invocation.trigger.input,
                Some(json!({"scope":scope, "generation":generation}))
            );
            tx.commit().await.unwrap();
        }
    }

    for (scope, (app_id, run_id)) in scopes.iter().enumerate() {
        service
            .for_app((*app_id).clone())
            .restart(
                &RequestId::mint(),
                run_id,
                crate::operations::RestartOptions::default(),
            )
            .await
            .unwrap();
        let mut tx = service.begin().await.unwrap();
        app::lock_app(&mut tx, app_id).await.unwrap();
        let previous = tx
            .database()
            .entity::<models::generations::Entity>()
            .unwrap()
            .find::<GenerationLifecycle>(
                models::generations::app_id
                    .eq(app_id.as_str())
                    .unwrap()
                    .and(models::generations::run_id.eq(*run_id).unwrap())
                    .and(models::generations::generation.eq(1_i64).unwrap()),
                FindOptions {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(previous[0].state, "completed");
        assert_eq!(previous[0].terminal_at, Some(now));
        let run = app::lock_run(&mut tx, app_id, run_id).await.unwrap();
        assert_eq!(run.integer("generation").unwrap(), 2);
        let invocation = frontier::invocation(&mut tx, app_id, &run).await.unwrap();
        assert_eq!(
            invocation.trigger.input,
            Some(json!({"scope":scope, "generation":1}))
        );
        assert!(invocation.journal.is_empty());
        tx.commit().await.unwrap();
    }
}
