#![expect(
    clippy::future_not_send,
    reason = "restart integrity tests own compio-local journal transactions"
)]

use super::super::management::fixture::{latest, started};
use super::super::*;
use crate::{
    deployment_holds::HoldScope,
    service::{app, store::Row},
};
use std::collections::BTreeMap;
use zeroship_core::workflow_coordination::{ManagementOutcome, RunState};
use zeroship_data_orm::{orm::Operation, value, Value};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            let store = sqlite_store(&directory.path().join("workflow.sqlite")).await;
            Box::pin($contract(Rc::new(store))).await;
        }

        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

case!(
    sqlite_started_restart_retries_retained_deployment_damage,
    postgres_started_restart_retries_retained_deployment_damage,
    damaged_identity
);
case!(
    sqlite_started_restart_uses_current_source_after_latest,
    postgres_started_restart_uses_current_source_after_latest,
    current_source
);
case!(
    sqlite_started_restart_receipt_replays_after_hold_damage,
    postgres_started_restart_receipt_replays_after_hold_damage,
    receipt_replay
);

async fn start(service: &WorkflowService, app: &AppId) -> String {
    service
        .fixture_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap()
        .id
}

fn applied() -> ManagementOutcome {
    ManagementOutcome::Applied {
        state: RunState::Queued,
    }
}

async fn active(service: &WorkflowService, app: &AppId) -> DeployRegistration {
    let mut tx = service.begin().await.unwrap();
    let deployment = app::active_deploy(&mut tx, app).await.unwrap();
    tx.commit().await.unwrap();
    deployment
}

async fn one(service: &WorkflowService, table: &str, filter: serde_json::Value) -> Row {
    let tx = service.begin().await.unwrap();
    let mut rows = journal_rows(&tx, table, filter).await;
    assert_eq!(rows.len(), 1, "fixture must select one {table} row");
    tx.commit().await.unwrap();
    rows.pop().unwrap()
}

async fn patch(service: &WorkflowService, table: &str, id: &str, changes: Value) {
    let tx = service.begin().await.unwrap();
    tx.database()
        .collection(&format!("__zeroship_workflow_{table}"))
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":id}),
            patch: changes.clone(),
            many: false,
        })
        .await
        .unwrap();
    let rows = journal_rows(&tx, table, json!({"id":id})).await;
    assert_eq!(rows.len(), 1);
    for (field, expected) in changes.as_object().unwrap() {
        assert_eq!(rows[0].0.get(field), Some(expected), "{table}.{field}");
    }
    tx.commit().await.unwrap();
}

async fn restore(service: &WorkflowService, table: &str, mut row: Value) {
    let id = row.as_object_mut().unwrap().shift_remove("id").unwrap();
    patch(service, table, id.as_str().unwrap(), row).await;
}

type Snapshot = BTreeMap<&'static str, Vec<Value>>;

async fn snapshot(service: &WorkflowService) -> Snapshot {
    let tx = service.begin().await.unwrap();
    let mut snapshot = BTreeMap::new();
    for table in [
        "runs",
        "generations",
        "job_publications",
        "management_receipts",
        "job_receipts",
        "requests",
        "outbox",
        "deploys",
        "deployment_holds",
    ] {
        snapshot.insert(
            table,
            journal_rows(&tx, table, json!({}))
                .await
                .into_iter()
                .map(|row| row.0)
                .collect(),
        );
    }
    tx.commit().await.unwrap();
    snapshot
}

async fn assert_generation(
    service: &WorkflowService,
    app: &AppId,
    run: &str,
    expected_generation: i64,
    expected_deploy: &str,
) {
    let row = one(service, "runs", json!({"app_id":app.as_str(),"id":run})).await;
    assert_eq!(row.integer("generation").unwrap(), expected_generation);
    assert_eq!(row.text("deploy_id").unwrap(), expected_deploy);
    let generation = one(
        service,
        "generations",
        json!({"app_id":app.as_str(),"run_id":run,"generation":expected_generation}),
    )
    .await;
    assert_eq!(generation.text("deploy_id").unwrap(), expected_deploy);
}

struct Damage {
    name: &'static str,
    table: &'static str,
    filter: serde_json::Value,
    patch: Value,
}

fn damage_cases(
    owner: &AppId,
    foreign: &AppId,
    run: &str,
    original: &DeployRegistration,
    alternative: &DeployRegistration,
) -> Vec<Damage> {
    let deploy_filter = json!({"app_id":owner.as_str(),"id":original.id});
    let hold_filter = json!({"app_id":owner.as_str(),"deploy_id":original.id});
    let generation_filter = json!({"app_id":owner.as_str(),"run_id":run,"generation":0});
    let mut bad_id = original.clone();
    bad_id.id.clone_from(&alternative.id);
    let mut bad_hash = original.clone();
    bad_hash.hash.clone_from(&alternative.hash);
    let mut missing_workflow = original.clone();
    missing_workflow.workflows.remove("Example");
    let mut damage = vec![
        Damage {
            name: "run and current source disagree",
            table: "runs",
            filter: json!({"app_id":owner.as_str(),"id":run}),
            patch: value!({"deploy_id":alternative.id}),
        },
        Damage {
            name: "current generation and run disagree",
            table: "generations",
            filter: generation_filter,
            patch: value!({"deploy_id":alternative.id}),
        },
    ];
    for (name, changes) in [
        (
            "manifest names another deployment",
            value!({"manifest":serde_json::to_string(&bad_id).unwrap()}),
        ),
        (
            "manifest hash disagrees",
            value!({"manifest":serde_json::to_string(&bad_hash).unwrap()}),
        ),
        (
            "workflow missing from manifest",
            value!({"manifest":serde_json::to_string(&missing_workflow).unwrap()}),
        ),
        ("manifest cannot be decoded", value!({"manifest":"{"})),
        ("registration hash is invalid", value!({"hash":"invalid"})),
        (
            "availability epoch is invalid",
            value!({"availability_epoch":-1}),
        ),
        ("deployment is unavailable", value!({"state":"unavailable"})),
        ("deployment is retiring", value!({"state":"retiring"})),
    ] {
        damage.push(Damage {
            name,
            table: "deploys",
            filter: deploy_filter.clone(),
            patch: changes,
        });
    }
    for (name, changes) in [
        ("hold is not held", value!({"state":"releasing"})),
        ("hold generation is invalid", value!({"generation":0})),
        (
            "hold belongs to another holder",
            value!({"holder_id":HoldScope::for_app(foreign.clone()).holder()}),
        ),
        (
            "queue hold cannot replace journal retention",
            value!({"holder_id":HoldScope::for_queue(owner.clone()).holder()}),
        ),
        (
            "hold hash disagrees",
            value!({"deploy_hash":alternative.hash}),
        ),
        ("hold hash is missing", value!({"deploy_hash":Value::Null})),
        (
            "hold is absent from app scope",
            value!({"app_id":foreign.as_str()}),
        ),
    ] {
        damage.push(Damage {
            name,
            table: "deployment_holds",
            filter: hold_filter.clone(),
            patch: changes,
        });
    }

    damage
}

async fn damaged_identity(store: Rc<OrmStore>) {
    let (service, owner, foreign, deployments) = registered_service(store.clone()).await;
    let original = active(&service, &owner).await;
    let alternative = deployments.deploy(&owner).await;
    assert_ne!(alternative.id, original.id);
    assert_ne!(alternative.hash, original.hash);
    service.retain_deploy(&owner, &alternative).await.unwrap();
    let journal = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap();
    let scope = journal.fixture_app(owner.clone());
    let run = start(&service, &owner).await;
    let damage = damage_cases(&owner, &foreign, &run, &original, &alternative);

    let mut generation = 0;
    let mut revision = 1;
    for mut damaged in damage {
        // Each successful repair becomes the source generation of the next attempt.
        if damaged.table == "generations" {
            damaged.filter["generation"] = json!(generation);
        }
        let stored = one(&service, damaged.table, damaged.filter).await;
        patch(
            &service,
            damaged.table,
            &stored.text("id").unwrap(),
            damaged.patch,
        )
        .await;
        let before = snapshot(&journal).await;
        let request = started(&owner, &run, revision);
        let result = scope.management_outcome(&request).await;
        assert!(
            result.is_err(),
            "{} must be retryable: {result:?}",
            damaged.name
        );
        assert_eq!(
            snapshot(&journal).await,
            before,
            "{} changed journal",
            damaged.name
        );
        restore(&service, damaged.table, stored.0).await;
        assert_eq!(
            scope.management_outcome(&request).await.unwrap(),
            applied(),
            "{}",
            damaged.name
        );
        generation += 1;
        revision += 1;
        assert_generation(&journal, &owner, &run, generation, &original.id).await;
    }

    let source = one(
        &journal,
        "generations",
        json!({"app_id":owner.as_str(),"run_id":run,"generation":generation}),
    )
    .await;
    let head = one(&journal, "runs", json!({"app_id":owner.as_str(),"id":run})).await;
    patch(
        &journal,
        "generations",
        &source.text("id").unwrap(),
        value!({"generation":-1}),
    )
    .await;
    patch(&journal, "runs", &run, value!({"generation":-1})).await;
    let before = snapshot(&journal).await;
    let request = started(&owner, &run, revision);
    assert!(matches!(
        scope.management_outcome(&request).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(snapshot(&journal).await, before);
    restore(&journal, "generations", source.0).await;
    restore(&journal, "runs", head.0).await;
    assert_eq!(scope.management_outcome(&request).await.unwrap(), applied());
    assert_generation(&journal, &owner, &run, generation + 1, &original.id).await;
}

async fn current_source(store: Rc<OrmStore>) {
    let (service, owner, _, deployments) = registered_service(store.clone()).await;
    let original = active(&service, &owner).await;
    let run = start(&service, &owner).await;
    let replacement = deployments.deploy(&owner).await;
    service.activate_deploy(&owner, &replacement).await.unwrap();
    let scope = service.fixture_app(owner.clone());
    assert_eq!(
        scope
            .management_outcome(&latest(&owner, &run, 1, &replacement))
            .await
            .unwrap(),
        applied()
    );
    assert_generation(&service, &owner, &run, 1, &replacement.id).await;

    let newer = deployments.deploy(&owner).await;
    service.activate_deploy(&owner, &newer).await.unwrap();
    let journal = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap();
    assert_eq!(
        journal
            .fixture_app(owner.clone())
            .management_outcome(&started(&owner, &run, 2))
            .await
            .unwrap(),
        applied()
    );
    assert_generation(&journal, &owner, &run, 2, &replacement.id).await;
    assert_eq!(active(&journal, &owner).await.id, newer.id);
    for (generation, deploy) in [(0, original.id), (1, replacement.id)] {
        let row = one(
            &journal,
            "generations",
            json!({"app_id":owner.as_str(),"run_id":run,"generation":generation}),
        )
        .await;
        assert_eq!(row.text("deploy_id").unwrap(), deploy);
    }
}

async fn receipt_replay(store: Rc<OrmStore>) {
    let (service, owner, _, _deployments) = registered_service(store.clone()).await;
    let deployment = active(&service, &owner).await;
    let run = start(&service, &owner).await;
    let journal = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap();
    let scope = journal.fixture_app(owner.clone());
    let completed = started(&owner, &run, 1);
    assert_eq!(
        scope.management_outcome(&completed).await.unwrap(),
        applied()
    );
    let hold = one(
        &journal,
        "deployment_holds",
        json!({"app_id":owner.as_str(),"deploy_id":deployment.id}),
    )
    .await;
    patch(
        &journal,
        "deployment_holds",
        &hold.text("id").unwrap(),
        value!({"state":"released"}),
    )
    .await;
    let before = snapshot(&journal).await;
    assert_eq!(
        scope.management_outcome(&completed).await.unwrap(),
        applied()
    );
    assert_eq!(snapshot(&journal).await, before);

    let pending = started(&owner, &run, 2);
    assert!(scope.management_outcome(&pending).await.is_err());
    assert_eq!(snapshot(&journal).await, before);
    restore(&journal, "deployment_holds", hold.0).await;
    assert_eq!(scope.management_outcome(&pending).await.unwrap(), applied());
    assert_generation(&journal, &owner, &run, 2, &deployment.id).await;
}
