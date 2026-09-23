use super::*;
use crate::service::{models, TaskAssignment, WorkerIdentity};
use std::collections::BTreeSet;
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow},
    value,
};

/// Claim material the host keeps for a dispatch, read back from the row the
/// runner authorizes against.
#[derive(FromRow)]
#[orm(entity = models::tasks)]
struct TaskClaim {
    worker: String,
    token_hash: String,
}

const REPLAY_INPUT: &str = r#"{"order": "checkout"}"#;

#[compio::test]
async fn sqlite_replay_input_keeps_claim_credentials_in_the_host() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    claim_credentials(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_replay_input_keeps_claim_credentials_in_the_host() {
    let fixture = PostgresFixture::start().await;
    claim_credentials(Rc::new(fixture.store.clone())).await;
}

/// Replay input is the only part of a dispatch that enters app code: the V8
/// executor serializes `TaskAssignment::invocation` straight into the isolate,
/// so every field it carries is readable by the creator workflow. Claiming a
/// run mints the lease credential beside the invocation the frontier builds;
/// this holds the two apart on the production claim path, on a first claim and
/// on a redispatch that can reach the released claim still on file.
///
/// It does not catch a credential the host hands over some other way, such as
/// an env var installed by the runtime loader or a payload read.
async fn claim_credentials(store: Rc<OrmStore>) {
    let (service, app, _foreign, _deployments) = registered_service(store.clone()).await;
    let worker = WorkerIdentity::new("replay-credential-worker".into()).unwrap();
    let objects = objects::Objects::new();
    let scope = service.fixture_app(app.clone());
    let started = scope
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input_ref: objects
                    .start_input(&scope, serde_json::from_str(REPLAY_INPUT).unwrap())
                    .await,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let first = service.poll(&worker).await.unwrap().unwrap();
    let mut secrets = claim_material(&store, &first, &worker).await;
    replay_input_excludes("first claim", &first, &app, &started.id, &secrets);

    // A released lease stays on file, so the builder can now reach a claim for
    // this run. The redispatch it produces still carries none of it.
    service
        .release(&worker, &first.id, &first.token)
        .await
        .unwrap();
    let second = service.poll(&worker).await.unwrap().unwrap();
    assert_ne!(second.id, first.id, "the redispatch must be a fresh claim");
    secrets.extend(claim_material(&store, &second, &worker).await);
    replay_input_excludes("redispatch", &second, &app, &started.id, &secrets);
}

async fn claim_material(
    store: &OrmStore,
    task: &TaskAssignment,
    worker: &WorkerIdentity,
) -> Vec<(&'static str, String)> {
    let tx = store.begin().await.unwrap();
    let claims = tx
        .database()
        .entity::<models::tasks::Entity>()
        .unwrap()
        .find::<TaskClaim>(
            models::tasks::id.eq(task.id.as_str()).unwrap(),
            FindOptions::default(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(claims.len(), 1, "the dispatch must have a claim on file");
    assert_eq!(claims[0].worker, worker.as_str());
    vec![
        ("task token", task.token.as_str().to_owned()),
        ("claim digest", claims[0].token_hash.clone()),
        ("worker identity", worker.as_str().to_owned()),
        ("dispatch id", task.id.clone()),
    ]
}

fn replay_input_excludes(
    claim: &str,
    task: &TaskAssignment,
    app: &AppId,
    run_id: &str,
    secrets: &[(&'static str, String)],
) {
    let encoded = serde_json::to_value(&task.invocation).unwrap();
    // Replay input a claim failed to build would satisfy every exclusion below.
    assert_eq!(encoded["appId"], json!(app.as_str()));
    assert_eq!(encoded["runId"], json!(run_id));
    assert_eq!(
        encoded["trigger"]["input"],
        serde_json::from_str::<serde_json::Value>(REPLAY_INPUT).unwrap()
    );

    let fields: BTreeSet<&str> = encoded
        .as_object()
        .expect("replay input serializes to an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        BTreeSet::from([
            "appId",
            "deployHash",
            "deployId",
            "generation",
            "journal",
            "phase",
            "runId",
            "trigger",
            "workflowName",
        ]),
        "{claim} replay input changed shape; app code reads every field it carries"
    );
    let trigger: BTreeSet<&str> = encoded["trigger"]
        .as_object()
        .expect("replay trigger serializes to an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        trigger.contains("input")
            && trigger.contains("startedAt")
            && trigger.is_subset(&BTreeSet::from([
                "input",
                "inputRef",
                "runId",
                "startedAt",
                "workflowName",
            ])),
        "{claim} replay trigger changed shape: {trigger:?}"
    );

    assert!(!secrets.is_empty(), "{claim} replay input was searched for nothing");
    let text = encoded.to_string();
    for (name, secret) in secrets {
        assert!(!secret.is_empty(), "no {name} to search {claim} replay input for");
        assert!(
            !text.contains(secret.as_str()),
            "{claim} replay input exposed the {name}"
        );
    }
}

#[compio::test]
async fn sqlite_recovery_refuses_a_foreign_task_reference() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    foreign_reference(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_recovery_refuses_a_foreign_task_reference() {
    let fixture = PostgresFixture::start().await;
    foreign_reference(Rc::new(fixture.store.clone())).await;
}

async fn foreign_reference(store: Rc<OrmStore>) {
    let (service, app, foreign, _deployments) = registered_service(store.clone()).await;
    let worker = WorkerIdentity::new("task-model-worker".into()).unwrap();
    let mut assignments = Vec::new();
    for app_id in [&app, &foreign] {
        let started = service
            .fixture_app(app_id.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
        let task = service.poll(&worker).await.unwrap().unwrap();
        assert_eq!(task.invocation.run_id, started.id);
        assignments.push(task);
    }
    let local = &assignments[0];
    let other = &assignments[1];
    let tx = store.begin().await.unwrap();
    tx.database()
        .collection(models::runs::Entity::COLLECTION)
        .unwrap()
        .update(
            value!({"app_id":app.as_str(), "id":local.invocation.run_id.clone()}),
            value!({"task_id":other.id.clone(), "due_at":0}),
        )
        .await
        .unwrap();
    tx.database()
        .collection(models::tasks::Entity::COLLECTION)
        .unwrap()
        .update(value!({"id":other.id.clone()}), value!({"deadline":0}))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let reopened = WorkflowService::open(store.clone(), Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    reopened
        .fixture_register(&app, leased_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    assert!(matches!(
        reopened.poll(&worker).await,
        Err(WorkflowServiceError::Internal(_))
    ));
    let heartbeat = reopened.heartbeat(&worker, &other.id, &other.token).await;
    assert!(
        matches!(heartbeat, Err(WorkflowServiceError::NotFound(_))),
        "a task outside the host's apps must be invisible: {heartbeat:?}"
    );
    let mut tx = store.begin().await.unwrap();
    for assignment in &assignments {
        let task = tx
            .database()
            .entity::<models::tasks::Entity>()
            .unwrap()
            .find::<models::TaskRecord>(
                models::tasks::id.eq(assignment.id.as_str()).unwrap(),
                FindOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(task.len(), 1);
        assert_eq!(task[0].state, "leased");
        assert_eq!(task[0].epoch, assignment.epoch);
        assert_eq!(task[0].app_id, assignment.invocation.app_id);
        assert_eq!(task[0].run_id, assignment.invocation.run_id);
    }
    let run = super::super::app::lock_run(&mut tx, &app, &local.invocation.run_id)
        .await
        .unwrap();
    assert_eq!(run.text("task_id").unwrap(), other.id);
    assert_eq!(run.integer("due_at").unwrap(), 0);
    tx.commit().await.unwrap();
}
