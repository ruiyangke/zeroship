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

/// The bytes one generation's result is stored as. Distinct per app scope and
/// per generation, so a status read that followed the wrong row reports a
/// descriptor that does not match.
fn result(scope: usize, generation: i64) -> Vec<u8> {
    serde_json::to_vec(&json!({"scope":scope, "generation":generation})).unwrap()
}

/// A run's result and its input both live in objects, so the generation row
/// carries only their descriptors. This is the column text the service decodes
/// one from.
fn stored_reference(scope: usize, generation: i64) -> String {
    serde_json::to_string(&output_reference(&result(scope, generation))).unwrap()
}

/// What a run of this scope and generation was started from, as bytes.
fn seed(scope: usize, generation: i64) -> Vec<u8> {
    serde_json::to_vec(&json!({"scope":scope, "generation":generation})).unwrap()
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
                input_ref: Some(output_reference(&seed(scope, 0))),
                ..Default::default()
            },
            None,
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
                "output_ref":stored_reference(scope, 0)}),
            )
            .await
            .unwrap();
        tx.database()
            .collection(models::runs::Entity::COLLECTION)
            .unwrap()
            .update(
                value!({"app_id":app_id.as_str(), "id":*run_id}),
                value!({"state":"completed", "terminal_at":now}),
            )
            .await
            .unwrap();
        let source = crate::service::continuations::member(&tx, app_id, run_id, 0)
            .await
            .unwrap();
        generations
            .insert(value!({
                "id":storage_id(), "app_id":app_id.as_str(), "run_id":*run_id, "generation":1, "deploy_id":deploy.id,
                "input_ref":serde_json::to_string(&output_reference(&seed(scope, 1))).unwrap(),
                "state":"completed", "started_at":now, "terminal_at":now,
                "output_ref":stored_reference(scope, 1),
            }))
            .await
            .unwrap();
        // Generation 1 is the head's restart successor, as production restart
        // records it before moving the run pointer.
        crate::service::continuations::restart(&tx, app_id, &source, run_id, 1)
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
                "record":crate::service::journal::encode_checkpoint(step, None, None).unwrap(),
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
                .fixture_app((*app_id).clone())
                .status(run_id)
                .await
                .unwrap();
            assert_eq!(status.state, crate::operations::RunState::Completed);
            let reference = output_reference(&result(scope, generation));
            assert_eq!(
                status.output,
                Some(json!({
                    "kind":"ref", "ref":format!("wfblob:sha256:{}", reference.hash),
                    "hash":reference.hash, "size":reference.size,
                    "contentType":reference.content_type,
                }))
            );
            let mut tx = service.begin().await.unwrap();
            app::lock_app(&mut tx, app_id).await.unwrap();
            let run = app::lock_run(&mut tx, app_id, run_id).await.unwrap();
            let invocation = frontier::invocation(&mut tx, app_id, &run).await.unwrap();
            assert_eq!(invocation.app_id, app_id.as_str());
            assert_eq!(invocation.run_id, *run_id);
            assert!(invocation.trigger.input.is_none());
            assert_eq!(
                invocation.trigger.input_ref,
                Some(output_reference(&seed(scope, generation)))
            );
            tx.commit().await.unwrap();
        }
    }

    for (scope, (app_id, run_id)) in scopes.iter().enumerate() {
        service
            .fixture_app((*app_id).clone())
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
        assert!(invocation.trigger.input.is_none());
        assert_eq!(
            invocation.trigger.input_ref,
            Some(output_reference(&seed(scope, 1)))
        );
        assert!(invocation.journal.is_empty());
        tx.commit().await.unwrap();
    }
}

#[compio::test]
async fn sqlite_model_replayed_failure_carries_only_the_bridge_keys() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    replayed_error_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_model_replayed_failure_carries_only_the_bridge_keys() {
    let fixture = PostgresFixture::start().await;
    replayed_error_contract(Rc::new(fixture.store.clone())).await;
}

/// The failure a dispatch replays, narrowed to what rebuilds the thrown object.
///
/// `wfDeserializeError` in `crates/zeroship-workflow-v8/js/dispatch.js` picks a
/// class by `type`, takes `message`, overwrites `stack` and copies `retryable`
/// when it is a boolean, and reads nothing else, so a key outside that set can
/// never reach a creator's `catch`. The dispatch view drops it rather than
/// carrying it to the worker to be discarded there.
///
/// The `journal::load` half is the control that keeps the narrowing where it
/// belongs: the stored checkpoint still holds the recorded value whole. Applied
/// at the storage site the same projection would take `retryable`, which is what
/// decides whether a failed step gets another execution, and the child-join
/// comparisons, which match a settled join against the error the child itself
/// recorded.
///
/// What this does not catch: the surviving `message` and `stack` are ordinary
/// strings under no cap, so this narrows the row rather than bounding it. It
/// says nothing about which producers write a key outside the set, nothing about
/// the stored row's own size, and nothing about the bridge continuing to read
/// only those four - that stays pinned by the bridge's own suite.
async fn replayed_error_contract(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let run_id = typed_id::new_workflow_run_id();
    let recorded = json!({
        "type":"PermanentError", "message":"order did not pass review",
        "stack":"PermanentError: order did not pass review\n    at review",
        "retryable":false, "strikes":3, "cause":{"code":"REVIEW_DECLINED"},
    });
    let crossing = json!({
        "type":"PermanentError", "message":"order did not pass review",
        "stack":"PermanentError: order did not pass review\n    at review",
        "retryable":false,
    });
    let withheld = ["strikes", "cause"];

    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &app_id).await.unwrap();
    let now = tx.now().await.unwrap();
    let deploy = app::active_deploy(&mut tx, &app_id).await.unwrap();
    app::insert_root_run(
        &mut tx,
        &app_id,
        &run_id,
        "Example",
        &deploy.id,
        &StartOptions::default(),
        None,
        now,
    )
    .await
    .unwrap();
    let mut step = StepCheckpoint::completed_run(0, "review", json!(null));
    step.state = "failed".into();
    step.output = None;
    step.error = Some(recorded.clone());
    tx.database()
        .collection(models::steps::Entity::COLLECTION)
        .unwrap()
        .insert(value!({
            "id":storage_id(), "app_id":app_id.as_str(), "run_id":run_id.as_str(), "generation":0,
            "ordinal":i64::from(step.ordinal), "name":step.name.clone(), "occurrence":0,
            "origin_generation":0, "kind":step.kind.clone(), "state":step.state.clone(),
            "record":journal::encode_checkpoint(&step, None, None).unwrap(),
        }))
        .await
        .unwrap();

    let stored = journal::load(&mut tx, &app_id, &run_id, 0).await.unwrap();
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(
        stored[0].error.as_ref(),
        Some(&recorded),
        "the stored checkpoint keeps the recorded failure whole: {stored:?}"
    );

    let run = app::lock_run(&mut tx, &app_id, &run_id).await.unwrap();
    let invocation = frontier::invocation(&mut tx, &app_id, &run).await.unwrap();
    assert_eq!(invocation.journal.len(), 1, "{invocation:?}");
    let replayed = invocation.journal[0]
        .error
        .clone()
        .expect("a failed step replays its failure");
    assert_eq!(replayed, crossing, "{replayed}");
    for key in withheld {
        assert!(
            recorded.get(key).is_some(),
            "{key} must be recorded for this case to bind: {recorded}"
        );
        assert!(
            replayed.get(key).is_none(),
            "{key} must not reach the bridge: {replayed}"
        );
    }
    tx.commit().await.unwrap();
}
