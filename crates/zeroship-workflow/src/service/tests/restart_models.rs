use super::*;
use crate::{
    engine::StepCheckpoint,
    operations::{RestartOptions, RestartTarget},
    service::{app, models},
};
use zeroship_data_orm::{
    budgets::MAX_INSERT_MANY_BATCH,
    orm::{Entity, Operation},
    sql::RowLimit,
    value, Value,
};

#[compio::test]
async fn sqlite_restart_copies_complete_scoped_prefix_and_rolls_back_failed_pages() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("workflow.sqlite")).await;
    contract(
        Rc::new(store),
        Fault::Sqlite(directory.path().join("zs-workflow.sqlite")),
    )
    .await;
}

#[compio::test]
async fn postgres_restart_copies_complete_scoped_prefix_and_rolls_back_failed_pages() {
    let fixture = PostgresFixture::start().await;
    contract(
        Rc::new(fixture.store.clone()),
        Fault::Postgres(fixture.admin_url.clone()),
    )
    .await;
}

enum Fault {
    Sqlite(std::path::PathBuf),
    Postgres(String),
}
impl Fault {
    async fn install(&self, last_retained: i32) {
        match self {
            Self::Sqlite(path) => sqlite_ddl(
                path,
                format!(
                    "CREATE TRIGGER restart_copy_fault BEFORE INSERT ON __zeroship_workflow_payload_refs
                     WHEN NEW.generation=2 AND NEW.slot='step' AND NEW.ordinal={last_retained}
                     BEGIN SELECT RAISE(ABORT,'restart copy fault'); END;"
                ),
            )
            .await,
            Self::Postgres(url) => connect(url)
                .await
                .batch_execute(&format!(
                    "CREATE FUNCTION customer.restart_copy_fault() RETURNS trigger LANGUAGE plpgsql AS $$
                     BEGIN IF NEW.generation=2 AND NEW.slot='step' AND NEW.ordinal={last_retained}
                     THEN RAISE EXCEPTION 'restart copy fault'; END IF; RETURN NEW; END $$;
                     CREATE TRIGGER restart_copy_fault BEFORE INSERT ON customer.__zeroship_workflow_payload_refs
                     FOR EACH ROW EXECUTE FUNCTION customer.restart_copy_fault();"
                ))
                .await
                .unwrap(),
        }
    }

    async fn remove(&self) {
        match self {
            Self::Sqlite(path) => {
                sqlite_ddl(path, "DROP TRIGGER restart_copy_fault".into()).await;
            }
            Self::Postgres(url) => connect(url)
                .await
                .batch_execute(
                    "DROP TRIGGER restart_copy_fault ON customer.__zeroship_workflow_payload_refs;
                     DROP FUNCTION customer.restart_copy_fault();",
                )
                .await
                .unwrap(),
        }
    }
}

async fn sqlite_ddl(path: &std::path::Path, sql: String) {
    let path = path.to_owned();
    // A rejected operation drops its transaction asynchronously. Keep the
    // runtime free to finish rollback while the fixture waits for the writer.
    compio::runtime::spawn_blocking(move || rusqlite::Connection::open(path)?.execute_batch(&sql))
        .await
        .unwrap()
        .unwrap();
}

async fn contract(store: Rc<OrmStore>, fault: Fault) {
    let (service, owner, foreign, _deployments) = registered_service(store).await;
    let run = typed_id::new_workflow_run_id();
    let foreign_run = typed_id::new_workflow_run_id();
    let neighbor = typed_id::new_workflow_run_id();
    let scopes = [
        (&owner, run.as_str()),
        (&foreign, foreign_run.as_str()),
        (&owner, neighbor.as_str()),
    ];
    let prefix = i32::try_from(RowLimit::default().get())
        .unwrap()
        .max(i32::try_from(MAX_INSERT_MANY_BATCH).unwrap())
        + 1;
    let mut tx = service.begin().await.unwrap();
    for app_id in [&owner, &foreign] {
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
        tx.database().collection(models::generations::Entity::COLLECTION).unwrap()
            .insert(value!({"id":storage_id(), "app_id":app_id.as_str(), "run_id":*run_id, "generation":1,
                "deploy_id":deploy.id, "input":"null", "state":"completed", "started_at":now, "terminal_at":now}))
            .await.unwrap();
        tx.database()
            .collection(models::runs::Entity::COLLECTION)
            .unwrap()
            .update(
                value!({"app_id":app_id.as_str(), "id":*run_id}),
                value!({"generation":1, "state":"completed", "terminal_at":now}),
            )
            .await
            .unwrap();
        for generation in [0, 1] {
            let task = typed_id::generate("wtk");
            let payload = typed_id::generate("wpl");
            tx.database()
                .collection(models::tasks::Entity::COLLECTION)
                .unwrap()
                .insert(
                    value!({"app_id":app_id.as_str(), "run_id":*run_id, "generation":generation,
                    "id":task.clone(), "worker":"fixture", "epoch":1, "token_hash":"fixture",
                    "deadline":0, "state":"completed", "created_at":now, "finished_at":now}),
                )
                .await
                .unwrap();
            tx.database().collection(models::payloads::Entity::COLLECTION).unwrap()
                .insert(value!({"app_id":app_id.as_str(), "run_id":*run_id, "generation":generation,
                    "id":payload.clone(), "task_id":task, "request_id":RequestId::mint().as_str(),
                    "hash":"a".repeat(64), "size":1, "state":"referenced", "created_at":now, "expires_at":0})).await.unwrap();
            let ordinals: Vec<_> = if scope == 0 && generation == 1 {
                (0..=prefix).collect()
            } else {
                vec![prefix]
            };
            let documents: Vec<_> = ordinals.iter().rev().map(|ordinal| {
                let mut step = StepCheckpoint::completed_run(*ordinal, "step", json!({"scope":scope, "generation":generation, "ordinal":ordinal}));
                step.name_occurrence = *ordinal;
                step.compensation_state = Some("pending".into());
                value!({"id":storage_id(), "app_id":app_id.as_str(), "run_id":*run_id, "generation":generation,
                    "ordinal":i64::from(*ordinal), "name":step.name.clone(), "occurrence":i64::from(*ordinal),
                    "origin_generation":0, "kind":step.kind.clone(), "state":step.state.clone(),
                    "record":serde_json::to_string(&step).unwrap(), "compensation_attempts":i64::from(*ordinal),
                    "compensation_due_at":now, "compensation_error":"retained diagnostic", "compensation_retry_ms":17})
            }).collect();
            insert(&tx, models::steps::Entity::COLLECTION, documents).await;
            let documents = [ ("input", -1), ("output", -1) ].into_iter()
                .chain(ordinals.into_iter().map(|ordinal| ("step", ordinal)))
                .map(|(slot, ordinal)| value!({"id":storage_id(), "app_id":app_id.as_str(), "run_id":*run_id,
                    "generation":generation, "slot":slot, "ordinal":i64::from(ordinal), "payload_id":payload.clone()}))
                .collect();
            insert(&tx, models::payload_refs::Entity::COLLECTION, documents).await;
        }
    }
    tx.commit().await.unwrap();

    let mut tx = service.begin().await.unwrap();
    let mut baseline = Vec::new();
    for (app_id, run_id) in scopes {
        for generation in [0, 1] {
            baseline.push(snapshot(&mut tx, app_id, run_id, generation).await);
        }
    }
    tx.commit().await.unwrap();

    let request = RequestId::mint();
    assert!(matches!(
        service
            .for_app(owner.clone())
            .restart(&RequestId::mint(), &foreign_run, RestartOptions::default())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let options = RestartOptions {
        from: Some(RestartTarget {
            name: "step".into(),
            occurrence: Some(prefix as u32),
        }),
        ..Default::default()
    };
    fault.install(prefix - 1).await;
    let error = service
        .for_app(owner.clone())
        .restart(&request, &run, options.clone())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            WorkflowServiceError::Internal(_) | WorkflowServiceError::Unavailable(_)
        ),
        "{error}"
    );
    let mut tx = service.begin().await.unwrap();
    let head = app::lock_run(&mut tx, &owner, &run).await.unwrap();
    assert_eq!(head.integer("generation").unwrap(), 1);
    assert_eq!(head.text("state").unwrap(), "completed");
    assert_eq!(
        snapshot(&mut tx, &owner, &run, 2).await,
        (Vec::new(), Vec::new())
    );
    assert_eq!(
        journal_count(
            &tx,
            "generations",
            json!({"app_id":owner.as_str(), "run_id":run.as_str(), "generation":2})
        )
        .await,
        0
    );
    assert_eq!(
        journal_count(
            &tx,
            "requests",
            json!({"app_id":owner.as_str(), "request_id":request.as_str()})
        )
        .await,
        0
    );
    tx.commit().await.unwrap();
    fault.remove().await;

    let result = service
        .for_app(owner.clone())
        .restart(&request, &run, options.clone())
        .await
        .unwrap();
    let retry = service
        .for_app(owner.clone())
        .restart(&request, &run, options)
        .await
        .unwrap();
    assert_eq!(result.run_id, retry.run_id);
    assert_eq!(result.pinned_to, retry.pinned_to);
    let mut tx = service.begin().await.unwrap();
    let head = app::lock_run(&mut tx, &owner, &run).await.unwrap();
    assert_eq!(head.integer("generation").unwrap(), 2);
    let mut expected = baseline[1].clone();
    expected
        .0
        .retain(|row| row["ordinal"].as_i64().unwrap() < i64::from(prefix));
    expected.1.retain(|(slot, ordinal, _)| {
        slot == "input" || (slot == "step" && *ordinal < i64::from(prefix))
    });
    let copied = snapshot(&mut tx, &owner, &run, 2).await;
    assert_eq!(copied.0.len(), prefix as usize);
    assert_eq!(copied, expected);
    for (scope, (app_id, run_id)) in scopes.iter().enumerate() {
        for generation in [0, 1] {
            assert_eq!(
                snapshot(&mut tx, app_id, run_id, generation).await,
                baseline[scope * 2 + generation as usize]
            );
        }
        if scope != 0 {
            assert_eq!(
                snapshot(&mut tx, app_id, run_id, 2).await,
                (Vec::new(), Vec::new())
            );
        }
    }
    tx.commit().await.unwrap();

    // A full restart keeps the input reference while discarding replay results.
    service
        .for_app(owner.clone())
        .restart(&RequestId::mint(), &run, RestartOptions::default())
        .await
        .unwrap();
    let mut tx = service.begin().await.unwrap();
    let (steps, refs) = snapshot(&mut tx, &owner, &run, 3).await;
    assert!(steps.is_empty());
    assert_eq!(
        refs,
        copied
            .1
            .into_iter()
            .filter(|(slot, _, _)| slot == "input")
            .collect::<Vec<_>>()
    );
    tx.commit().await.unwrap();
}

async fn insert(tx: &Transaction, collection: &str, documents: Vec<Value>) {
    for batch in documents.chunks(MAX_INSERT_MANY_BATCH) {
        tx.database()
            .collection(collection)
            .unwrap()
            .execute(Operation::InsertMany {
                documents: Value::Array(batch.to_vec()),
            })
            .await
            .unwrap();
    }
}

type Snapshot = (Vec<serde_json::Value>, Vec<(String, i64, String)>);
async fn snapshot(tx: &mut Transaction, app: &AppId, run: &str, generation: i64) -> Snapshot {
    let filter = json!({"app_id":app.as_str(), "run_id":run, "generation":generation});
    let mut steps = journal_rows(tx, "steps", filter.clone()).await;
    steps.sort_by_key(|row| row.integer("ordinal").unwrap());
    let steps = steps.into_iter().map(|row| json!({
        "ordinal":row.integer("ordinal").unwrap(), "name":row.text("name").unwrap(),
        "occurrence":row.integer("occurrence").unwrap(), "origin":row.integer("origin_generation").unwrap(),
        "kind":row.text("kind").unwrap(), "state":row.text("state").unwrap(), "record":row.text("record").unwrap(),
        "attempts":row.integer("compensation_attempts").unwrap(), "due":row.optional_integer("compensation_due_at").unwrap(),
        "error":row.optional_text("compensation_error").unwrap(), "retry":row.integer("compensation_retry_ms").unwrap(),
    })).collect();
    let mut refs = journal_rows(tx, "payload_refs", filter).await;
    refs.sort_by_key(|row| (row.text("slot").unwrap(), row.integer("ordinal").unwrap()));
    (
        steps,
        refs.into_iter()
            .map(|row| {
                (
                    row.text("slot").unwrap(),
                    row.integer("ordinal").unwrap(),
                    row.text("payload_id").unwrap(),
                )
            })
            .collect(),
    )
}
