//! Bytes staged for an app before any run exists.
//!
//! A payload's `run_id`, `generation` and `task_id` say WHERE a staged row
//! waits for an owner; durable ownership is an edge in `payload_refs`. These
//! cases drive the shape that has no location at all -- stage first, start the
//! run afterwards, let the run acquire the bytes -- and the two controls that
//! keep the new ownership arm honest: another app cannot reach them, and a
//! retried upload does not stage them twice.

use super::*;
use crate::service::store::Row;

macro_rules! paired {
    ($sqlite:ident, $postgres:ident, $case:path) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            Box::pin($case(Rc::new(
                sqlite_store(&directory.path().join("journal.sqlite")).await,
            )))
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let database = PostgresFixture::start().await;
            Box::pin($case(Rc::new(database.store.clone()))).await;
        }
    };
}

paired!(
    sqlite_ownerless_payload_attaches_to_a_run_started_after_it,
    postgres_ownerless_payload_attaches_to_a_run_started_after_it,
    attachment
);
paired!(
    sqlite_ownerless_payload_is_not_attachable_by_another_app,
    postgres_ownerless_payload_is_not_attachable_by_another_app,
    foreign_app
);
paired!(
    sqlite_ownerless_staging_deduplicates_on_the_request_alone,
    postgres_ownerless_staging_deduplicates_on_the_request_alone,
    dedupe
);
paired!(
    sqlite_ownerless_payload_skips_the_composite_keys_a_located_one_obeys,
    postgres_ownerless_payload_skips_the_composite_keys_a_located_one_obeys,
    match_simple
);

/// Bytes staged while the app has no run at all, attached by a run created
/// afterwards and read back through the edge that attachment recorded.
async fn attachment(store: Rc<OrmStore>) {
    let objects = Objects::new();
    let (service, app, _, _deployments) = registered_service(store.clone()).await;
    let data = br#"{"seed":"ownerless"}"#;
    let seed = reference(data);

    // Nothing has been started, so there is no generation row any location
    // column could name. Staging has to succeed anyway or the whole shape is
    // unreachable.
    assert!(payload_ids(&store, &app).await.is_empty());
    let staged = service
        .stage_app_payload(&app, &RequestId::mint(), seed.clone(), objects.upload(data))
        .await
        .unwrap();
    assert_eq!(staged.reference, seed);
    let row = payload_row(&store, &app, &staged.id).await;
    assert_eq!(row.text("state").unwrap(), "staged");
    for column in ["run_id", "task_id"] {
        assert_eq!(
            row.optional_text(column).unwrap(),
            None,
            "{column} must stay unset on an ownerless staging"
        );
    }
    assert_eq!(row.optional_integer("generation").unwrap(), None);
    assert!(objects.exists(&app, &staged.id));

    // The run arrives after the bytes and claims them by descriptor.
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("ownerless-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted","outputRef":seed}])),
        )
        .await
        .unwrap();

    // Ownership moved to the edge and ONLY to the edge: attachment does not
    // backfill the location columns, because they were never the owner.
    let row = payload_row(&store, &app, &staged.id).await;
    assert_eq!(row.text("state").unwrap(), "referenced");
    assert_eq!(row.optional_text("run_id").unwrap(), None);
    assert_eq!(row.optional_integer("generation").unwrap(), None);
    let edges = edges_for(&store, &app, &staged.id).await;
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].text("run_id").unwrap(), run.id);
    assert_eq!(edges[0].text("slot").unwrap(), "output");
    assert_eq!(edges[0].integer("generation").unwrap(), 0);

    assert_eq!(
        scope.read_output(&run.id, objects.open()).await.unwrap(),
        data
    );
    assert_eq!(
        scope
            .read_payload(&run.id, 0, PayloadSlot::Output, objects.open())
            .await
            .unwrap(),
        data
    );
}

/// The ownership arm for an unowned staging inherits its tenant scope from
/// `app_id` and its content identity from the hash triple, and nothing else.
/// Two runs differing in exactly one variable -- the app -- reach for the same
/// descriptor here; only the owning one may have it.
async fn foreign_app(store: Rc<OrmStore>) {
    let objects = Objects::new();
    let (service, app, other, _deployments) = registered_service(store.clone()).await;
    let data = br#"{"seed":"scoped"}"#;
    let seed = reference(data);
    let staged = service
        .stage_app_payload(&app, &RequestId::mint(), seed.clone(), objects.upload(data))
        .await
        .unwrap();
    let worker = WorkerIdentity::new("ownerless-worker".into()).unwrap();

    let foreign = service.fixture_app(other.clone());
    foreign
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let intruder = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(intruder.invocation.app_id, other.as_str());
    let refused = service
        .complete(
            &worker,
            &intruder.id,
            &intruder.token,
            execution(json!([{"kind":"RunCompleted","outputRef":seed}])),
        )
        .await;
    assert!(
        matches!(refused, Err(WorkflowServiceError::NotFound(_))),
        "{refused:?}"
    );
    // The refusal leaves the payload untouched: it was never the other app's
    // to consume, and no edge was recorded for it.
    assert_eq!(
        payload_row(&store, &app, &staged.id)
            .await
            .text("state")
            .unwrap(),
        "staged"
    );
    assert!(edges_for(&store, &app, &staged.id).await.is_empty());

    // The positive half, so the refusal above rests on the app scope rather
    // than on the descriptor being unreachable from any run.
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let owner = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(owner.invocation.app_id, app.as_str());
    service
        .complete(
            &worker,
            &owner.id,
            &owner.token,
            execution(json!([{"kind":"RunCompleted","outputRef":seed}])),
        )
        .await
        .unwrap();
    assert_eq!(
        scope.read_output(&run.id, objects.open()).await.unwrap(),
        data
    );
}

/// `request_id` is the whole idempotency key when no task holds the staging,
/// so the lookup has to drop its task term. A retry whose lookup still named a
/// task would miss the row staged with none, stage a second one, and stop
/// deduplicating without ever failing.
async fn dedupe(store: Rc<OrmStore>) {
    let objects = Objects::new();
    let (service, app, _, _deployments) = registered_service(store.clone()).await;
    let data = br#"{"seed":"retried"}"#;
    let seed = reference(data);
    let request = RequestId::mint();

    let first = service
        .stage_app_payload(&app, &request, seed.clone(), objects.upload(data))
        .await
        .unwrap();
    let retry = service
        .stage_app_payload(&app, &request, seed.clone(), objects.upload(data))
        .await
        .unwrap();
    assert_eq!(
        first, retry,
        "a retried upload must resolve to the row it already staged"
    );
    assert_eq!(
        payload_ids(&store, &app).await,
        vec![first.id.clone()],
        "the retry must not have staged a second row"
    );

    // The control: a different request is a different upload, so the count
    // above is one this fixture can actually move.
    let second = service
        .stage_app_payload(&app, &RequestId::mint(), seed, objects.upload(data))
        .await
        .unwrap();
    assert_ne!(second.id, first.id);
    let mut expected = vec![first.id, second.id];
    expected.sort();
    assert_eq!(payload_ids(&store, &app).await, expected);
}

/// Both dialects default to MATCH SIMPLE, under which a composite key carrying
/// a NULL is satisfied without a lookup. So the keys into `generations` and
/// `tasks` stay declared and stay enforced: an ownerless row skips them, and a
/// row that names a run still has to find what it names.
async fn match_simple(store: Rc<OrmStore>) {
    let (_service, app, _, _deployments) = registered_service(store.clone()).await;
    let record = |run_id: serde_json::Value, generation: serde_json::Value| {
        json!({
            "id": typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX),
            "app_id": app.as_str(), "run_id": run_id, "generation": generation,
            "task_id": serde_json::Value::Null, "request_id": RequestId::mint().as_str(),
            "hash": "0".repeat(64), "size": 0, "content_type": serde_json::Value::Null,
            "state": "staged", "created_at": 0, "expires_at": 0,
        })
    };

    let tx = store.begin().await.unwrap();
    journal_insert(&tx, "payloads", record(json!(null), json!(null)))
        .await
        .expect("a NULL composite key is satisfied without a lookup");
    tx.commit().await.unwrap();
    assert_eq!(payload_ids(&store, &app).await.len(), 1);

    // The control differs in exactly the two columns that carry the key: the
    // run was never started, so the key must be looked up and must fail.
    let tx = store.begin().await.unwrap();
    let refused = journal_insert(
        &tx,
        "payloads",
        record(json!("wfr_absent_from_this_journal"), json!(0)),
    )
    .await;
    assert!(
        refused.is_err(),
        "a located payload must still find its generation: {refused:?}"
    );
    drop(tx);
    assert_eq!(
        payload_ids(&store, &app).await.len(),
        1,
        "the refused insert must not have landed"
    );
}

async fn payload_row(store: &Rc<OrmStore>, app: &AppId, id: &str) -> Row {
    let tx = store.begin().await.unwrap();
    let mut rows = journal_rows(&tx, "payloads", json!({"app_id":app.as_str(), "id":id})).await;
    tx.commit().await.unwrap();
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

async fn edges_for(store: &Rc<OrmStore>, app: &AppId, payload: &str) -> Vec<Row> {
    let tx = store.begin().await.unwrap();
    let rows = journal_rows(
        &tx,
        "payload_refs",
        json!({"app_id":app.as_str(), "payload_id":payload}),
    )
    .await;
    tx.commit().await.unwrap();
    rows
}

/// Every payload this app holds, sorted, so a count assertion names which rows
/// it counted rather than only how many.
async fn payload_ids(store: &Rc<OrmStore>, app: &AppId) -> Vec<String> {
    let tx = store.begin().await.unwrap();
    let rows = journal_rows(&tx, "payloads", json!({"app_id":app.as_str()})).await;
    tx.commit().await.unwrap();
    let mut ids: Vec<String> = rows.iter().map(|row| row.text("id").unwrap()).collect();
    ids.sort();
    ids
}
