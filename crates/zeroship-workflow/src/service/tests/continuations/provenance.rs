#![expect(
    clippy::future_not_send,
    reason = "continuation tests own compio-local creator journals"
)]

use super::*;
use crate::{
    engine::WorkflowOutputRef,
    operations::{RestartOptions, RestartTarget, RunState},
    service::{tests::objects::Objects, TaskAssignment, WorkerIdentity},
};
use std::collections::BTreeMap;

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
    sqlite_completed_child_provenance_survives_restarts_and_prefix_copy,
    postgres_completed_child_provenance_survives_restarts_and_prefix_copy,
    retained_provenance
);
case!(
    sqlite_substituted_child_member_cannot_rewrite_consumed_provenance,
    postgres_substituted_child_member_cannot_rewrite_consumed_provenance,
    substituted_provenance
);
case!(
    sqlite_collection_keeps_consumed_child_provenance,
    postgres_collection_keeps_consumed_child_provenance,
    collected_provenance
);

const ORIGINAL: &[u8] = br#"{"value":"original"}"#;
const REPLACEMENT: &[u8] = br#"{"value":"replacement"}"#;
const ABANDONED: &[u8] = br#"{"value":"abandoned"}"#;

#[derive(Clone, Copy)]
enum OutputKind {
    Inline,
    Reference,
}

struct Family {
    parent: String,
    child: String,
    accepted: String,
    result: String,
    record: String,
}

fn worker() -> WorkerIdentity {
    WorkerIdentity::new("continuation-provenance-worker".into()).unwrap()
}

fn output_reference(data: &[u8]) -> WorkflowOutputRef {
    WorkflowOutputRef {
        hash: crate::service::hash(data),
        size: i64::try_from(data.len()).unwrap(),
        content_type: Some("application/json".into()),
    }
}

async fn complete_child(
    service: &WorkflowService,
    objects: &Objects,
    task: &TaskAssignment,
    kind: OutputKind,
    data: &'static [u8],
) {
    let outcome = match kind {
        OutputKind::Inline => json!({
            "kind":"RunCompleted", "output":serde_json::from_slice::<serde_json::Value>(data).unwrap()
        }),
        OutputKind::Reference => {
            let reference = output_reference(data);
            service
                .stage_payload(
                    &worker(),
                    &task.id,
                    &task.token,
                    &RequestId::mint(),
                    reference.clone(),
                    objects.upload(data),
                )
                .await
                .unwrap();
            json!({"kind":"RunCompleted", "outputRef":reference})
        }
    };
    service
        .complete(
            &worker(),
            &task.id,
            &task.token,
            execution(json!([outcome])),
        )
        .await
        .unwrap();
}

async fn completed_family(
    service: &WorkflowService,
    objects: &Objects,
    app_id: &AppId,
    kind: OutputKind,
) -> Family {
    let scope = service.fixture_app(app_id.clone());
    let parent = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker()).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, parent.id);
    service
        .complete(
            &worker(),
            &task.id,
            &task.token,
            execution(json!([{
                "kind":"Child", "ordinal":0, "name":"child", "childWorkflowName":"Child",
                "input":{}, "options":{}
            }])),
        )
        .await
        .unwrap();
    let accepted = service.poll(&worker()).await.unwrap().unwrap();
    assert_ne!(accepted.invocation.run_id, parent.id);
    service
        .complete(
            &worker(),
            &accepted.id,
            &accepted.token,
            execution(json!([{"kind":"ContinueAsNew", "input":{}}])),
        )
        .await
        .unwrap();
    let child = service.poll(&worker()).await.unwrap().unwrap();
    assert_eq!(child.invocation.workflow_name, "Child");
    assert_ne!(child.invocation.run_id, accepted.invocation.run_id);
    complete_child(service, objects, &child, kind, ORIGINAL).await;
    assert_eq!(deliver_propagations(&scope).await.len(), 1);
    let resumed = service.poll(&worker()).await.unwrap().unwrap();
    assert_eq!(resumed.invocation.run_id, parent.id);
    assert_eq!(
        resumed.invocation.journal[0].child_run_id.as_deref(),
        Some(child.invocation.run_id.as_str())
    );
    assert_output(service, objects, &resumed, kind, ORIGINAL).await;
    service
        .complete(
            &worker(),
            &resumed.id,
            &resumed.token,
            execution(json!([
                {"kind":"StepCompleted", "ordinal":1, "name":"resume", "output":null},
                {"kind":"RunCompleted", "output":null}
            ])),
        )
        .await
        .unwrap();
    let step = child_step(service, app_id, &parent.id, 0).await;
    Family {
        parent: parent.id,
        child: child.invocation.run_id,
        accepted: step.text("child_member_id").unwrap(),
        result: step.text("child_result_member_id").unwrap(),
        record: step.text("record").unwrap(),
    }
}

async fn child_step(
    service: &WorkflowService,
    app_id: &AppId,
    parent: &str,
    generation: i64,
) -> crate::service::store::Row {
    let tx = service.begin().await.unwrap();
    let mut rows = journal_rows(
        &tx,
        "steps",
        json!({"app_id":app_id.as_str(),"run_id":parent,"generation":generation,"ordinal":0}),
    )
    .await;
    assert_eq!(rows.len(), 1);
    tx.commit().await.unwrap();
    rows.pop().unwrap()
}

async fn assert_output(
    service: &WorkflowService,
    objects: &Objects,
    task: &TaskAssignment,
    kind: OutputKind,
    data: &[u8],
) {
    let step = &task.invocation.journal[0];
    assert_eq!(step.state, "completed");
    match kind {
        OutputKind::Inline => {
            assert_eq!(step.output, Some(serde_json::from_slice(data).unwrap()));
            assert!(step.output_ref.is_none());
        }
        OutputKind::Reference => {
            assert!(step.output.is_none());
            assert_eq!(step.output_ref, Some(output_reference(data)));
            assert_eq!(
                service
                    .read_task_payload(
                        &worker(),
                        &task.id,
                        &task.token,
                        &output_reference(data),
                        objects.open(),
                    )
                    .await
                    .unwrap(),
                data
            );
        }
    }
}

fn prefix() -> RestartOptions {
    RestartOptions {
        from: Some(RestartTarget {
            name: "resume".into(),
            occurrence: None,
        }),
        ..Default::default()
    }
}

async fn retained_provenance(store: Rc<OrmStore>) {
    for kind in [OutputKind::Inline, OutputKind::Reference] {
        let objects = Objects::new();
        let (service, app_id, _, _deployments) = registered_service(store.clone()).await;
        let family = completed_family(&service, &objects, &app_id, kind).await;
        assert_ne!(family.accepted, family.result);
        let scope = service.fixture_app(app_id.clone());
        scope
            .restart(&RequestId::mint(), &family.child, RestartOptions::default())
            .await
            .unwrap();
        let child = service.poll(&worker()).await.unwrap().unwrap();
        assert_eq!(child.invocation.run_id, family.child);
        assert_eq!(child.generation, 1);
        complete_child(&service, &objects, &child, kind, REPLACEMENT).await;
        assert_eq!(
            scope.status(&family.child).await.unwrap().state,
            RunState::Completed
        );
        scope
            .restart(&RequestId::mint(), &family.parent, prefix())
            .await
            .unwrap();
        let parent = service.poll(&worker()).await.unwrap().unwrap();
        assert_eq!(parent.invocation.run_id, family.parent);
        assert_eq!(parent.generation, 1);
        assert_eq!(parent.invocation.journal.len(), 1);
        assert_eq!(
            parent.invocation.journal[0].child_run_id.as_deref(),
            Some(family.child.as_str())
        );
        assert_output(&service, &objects, &parent, kind, ORIGINAL).await;
        for generation in [0, 1] {
            let step = child_step(&service, &app_id, &family.parent, generation).await;
            assert_eq!(step.text("child_member_id").unwrap(), family.accepted);
            assert_eq!(step.text("child_result_member_id").unwrap(), family.result);
            assert_eq!(step.text("record").unwrap(), family.record);
        }
        if matches!(kind, OutputKind::Reference) {
            let tx = service.begin().await.unwrap();
            let references = journal_rows(
                &tx,
                "payload_refs",
                json!({
                    "app_id":app_id.as_str(),"run_id":family.parent,"slot":"step","ordinal":0
                }),
            )
            .await;
            assert_eq!(references.len(), 2);
            assert_eq!(
                references[0].text("payload_id").unwrap(),
                references[1].text("payload_id").unwrap()
            );
            tx.commit().await.unwrap();
        }
        service
            .complete(
                &worker(),
                &parent.id,
                &parent.token,
                execution(json!([{"kind":"RunCompleted"}])),
            )
            .await
            .unwrap();
    }
}

/// Delivered collection after a consumed child output was restarted and copied
/// into the parent's replay prefix deletes only the abandoned preparation.
async fn collected_provenance(store: Rc<OrmStore>) {
    use crate::service::tests::payloads::collection::fixture::{options, Grant};
    let objects = Objects::new();
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let family = completed_family(&service, &objects, &app_id, OutputKind::Reference).await;
    let scope = service.fixture_app(app_id.clone());
    scope
        .restart(&RequestId::mint(), &family.child, RestartOptions::default())
        .await
        .unwrap();
    let child = service.poll(&worker()).await.unwrap().unwrap();
    assert_eq!(child.invocation.run_id, family.child);
    complete_child(&service, &objects, &child, OutputKind::Reference, REPLACEMENT).await;
    scope
        .restart(&RequestId::mint(), &family.parent, prefix())
        .await
        .unwrap();
    let parent = service.poll(&worker()).await.unwrap().unwrap();
    assert_eq!(parent.invocation.run_id, family.parent);
    let abandoned = service
        .stage_payload(
            &worker(),
            &parent.id,
            &parent.token,
            &RequestId::mint(),
            output_reference(ABANDONED),
            objects.upload(ABANDONED),
        )
        .await
        .unwrap()
        .id;
    // Expire every preparation, including the promoted outputs both child
    // generations produced, so only references can keep an object.
    let tx = service.begin().await.unwrap();
    let expired = tx
        .database()
        .entity::<models::payloads::Entity>()
        .unwrap()
        .update_many(
            models::payloads::app_id.eq(app_id.as_str()).unwrap(),
            models::payloads::expires_at.set(0_i64).unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(expired, 3, "original, replacement and abandoned payloads");
    let tables = [
        "continuation_heads",
        "continuation_members",
        "steps",
        "payload_refs",
    ];
    let retained = |state: &Snapshot| -> Snapshot {
        tables
            .iter()
            .map(|table| (*table, state[*table].clone()))
            .collect()
    };
    let before = retained(&snapshot(&service, &app_id).await);
    assert!(before.values().all(|rows| !rows.is_empty()));

    scope
        .collect_job(&Grant::new(&app_id), options(16), &objects)
        .await
        .unwrap();

    assert_eq!(retained(&snapshot(&service, &app_id).await), before);
    let tx = service.begin().await.unwrap();
    let payloads = journal_rows(&tx, "payloads", json!({"app_id":app_id.as_str()})).await;
    tx.commit().await.unwrap();
    assert_eq!(payloads.len(), 3);
    for payload in payloads {
        let id = payload.text("id").unwrap();
        let stored = objects.exists(&app_id, &id);
        if id == abandoned {
            assert_eq!(payload.text("state").unwrap(), "deleted");
            assert!(!stored, "collection must delete the abandoned object");
        } else {
            assert_ne!(payload.text("state").unwrap(), "deleted");
            assert!(stored, "collection must keep referenced child output");
        }
    }
    assert_output(&service, &objects, &parent, OutputKind::Reference, ORIGINAL).await;
    service
        .complete(
            &worker(),
            &parent.id,
            &parent.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
}

type Snapshot = BTreeMap<&'static str, Vec<zeroship_data_orm::Value>>;

async fn snapshot(service: &WorkflowService, app_id: &AppId) -> Snapshot {
    let tx = service.begin().await.unwrap();
    let mut state = Snapshot::new();
    for table in [
        "app_state",
        "runs",
        "generations",
        "continuation_heads",
        "continuation_members",
        "steps",
        "waits",
        "subscriptions",
        "tasks",
        "requests",
        "job_publications",
        "outbox",
        "payloads",
        "payload_refs",
    ] {
        state.insert(
            table,
            journal_rows(&tx, table, json!({"app_id":app_id.as_str()}))
                .await
                .into_iter()
                .map(|row| row.0)
                .collect(),
        );
    }
    tx.commit().await.unwrap();
    state
}

async fn substituted_provenance(store: Rc<OrmStore>) {
    for kind in [OutputKind::Inline, OutputKind::Reference] {
        let objects = Objects::new();
        let (service, app_id, _, _deployments) = registered_service(store.clone()).await;
        let family = completed_family(&service, &objects, &app_id, kind).await;
        let scope = service.fixture_app(app_id.clone());
        scope
            .restart(&RequestId::mint(), &family.child, RestartOptions::default())
            .await
            .unwrap();
        let tx = service.begin().await.unwrap();
        let generations = journal_rows(
            &tx,
            "generations",
            json!({
                "app_id":app_id.as_str(),"run_id":family.child,"generation":1
            }),
        )
        .await;
        assert_eq!(generations.len(), 1);
        let replacement = generations[0].text("id").unwrap();
        assert_ne!(replacement, family.result);
        tx.commit().await.unwrap();
        for completed in [false, true] {
            if completed {
                let child = service.poll(&worker()).await.unwrap().unwrap();
                assert_eq!(child.invocation.run_id, family.child);
                complete_child(&service, &objects, &child, kind, ORIGINAL).await;
            }
            assert_rejected_substitution(
                &service,
                &app_id,
                &family,
                "child_result_member_id",
                &replacement,
                &family.result,
            )
            .await;
        }
        assert_rejected_substitution(
            &service,
            &app_id,
            &family,
            "child_member_id",
            &family.result,
            &family.accepted,
        )
        .await;
        scope
            .restart(&RequestId::mint(), &family.parent, prefix())
            .await
            .unwrap();
        let parent = service.poll(&worker()).await.unwrap().unwrap();
        assert_eq!(parent.invocation.run_id, family.parent);
        assert_output(&service, &objects, &parent, kind, ORIGINAL).await;
        service
            .complete(
                &worker(),
                &parent.id,
                &parent.token,
                execution(json!([{"kind":"RunCompleted"}])),
            )
            .await
            .unwrap();
    }
}

async fn assert_rejected_substitution(
    service: &WorkflowService,
    app_id: &AppId,
    family: &Family,
    column: &str,
    replacement: &str,
    original: &str,
) {
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "steps",
        json!({
            "app_id":app_id.as_str(),"run_id":family.parent,"generation":0,"ordinal":0
        }),
        json!({(column):replacement}),
    )
    .await;
    tx.commit().await.unwrap();
    let before = snapshot(service, app_id).await;
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, app_id).await.unwrap();
    assert!(
        matches!(
            journal::load(&mut tx, app_id, &family.parent, 0).await,
            Err(WorkflowServiceError::Internal(_))
        ),
        "changed {column} was accepted during replay"
    );
    tx.commit().await.unwrap();
    assert!(
        matches!(
            service
                .fixture_app(app_id.clone())
                .restart(&RequestId::mint(), &family.parent, prefix())
                .await,
            Err(WorkflowServiceError::Internal(_))
        ),
        "changed {column} authorized prefix replay"
    );
    assert_eq!(snapshot(service, app_id).await, before);
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "steps",
        json!({
            "app_id":app_id.as_str(),"run_id":family.parent,"generation":0,"ordinal":0
        }),
        json!({(column):original}),
    )
    .await;
    tx.commit().await.unwrap();
}
