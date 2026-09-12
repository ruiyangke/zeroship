use super::*;
use crate::{
    engine::StepCheckpoint,
    service::{app, journal, models},
};
use zeroship_data_orm::{
    orm::{Entity, Operation},
    sql::{compile::MAX_INSERT_MANY_BATCH, RowLimit},
    value, Value,
};

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
    let (service, first_app, second_app) = registered_service(store).await;
    let shared_run = typed_id::new_workflow_run_id();
    let other_run = typed_id::new_workflow_run_id();
    let scopes = [
        (&first_app, shared_run.as_str()),
        (&second_app, shared_run.as_str()),
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
            &StartOptions::default(),
            now,
        )
        .await
        .unwrap();
        let generations = tx
            .database()
            .collection(models::generations::Entity::COLLECTION)
            .unwrap();
        generations.update(
            value!({"app_id":app_id.as_str(), "run_id":*run_id, "generation":0}),
            value!({"state":"completed", "output":json!({"scope":scope, "generation":0}).to_string()}),
        ).await.unwrap();
        generations
            .insert(value!({
                "app_id":app_id.as_str(), "run_id":*run_id, "generation":1, "deploy_id":deploy.id,
                "input":"null", "state":"completed", "started_at":now,
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
                "app_id":app_id.as_str(), "run_id":*run_id, "generation":generation,
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

    let history = journal::load(&mut tx, &first_app, &shared_run, 1)
        .await
        .unwrap();
    assert_eq!(history.len(), count as usize);
    for (ordinal, step) in history.iter().enumerate() {
        assert_eq!(step.ordinal, ordinal as i32);
        assert_eq!(step.output, Some(json!(ordinal)));
    }
    let mut changed = history.last().unwrap().clone();
    changed.output = Some(json!("updated"));
    journal::update(&mut tx, &first_app, &shared_run, 1, &changed)
        .await
        .unwrap();
    let history = journal::load(&mut tx, &first_app, &shared_run, 1)
        .await
        .unwrap();
    assert_eq!(history.len(), count as usize);
    assert_eq!(history.last().unwrap().output, changed.output);
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

    // Reusing a run ID across apps and moving the head across generations
    // exercises every component of the current-generation join.
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
        }
    }
}
