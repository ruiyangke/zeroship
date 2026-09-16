use super::*;
use crate::{
    deployment_holds::HoldScope,
    service::{
        IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
    },
};
use zeroship_data_orm::{value, Value};

fn changed_schedule(registration: &DeployRegistration) -> DeployRegistration {
    let mut changed = registration.clone();
    changed.schedules.push(ScheduleRegistration {
        name: "InjectedSchedule".into(),
        workflow_name: "Example".into(),
        schedule: ScheduleTiming::Interval {
            interval_ms: 60_000,
            anchor: IntervalAnchor::Deploy,
        },
        input: json!({"changed":true}),
        overlap: ScheduleOverlap::Allow,
        catch_up: ScheduleCatchUp::Skip,
    });
    changed
}

pub(super) async fn changed_expected(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let before = fixture.snapshot().await;
    let mut hash = fixture.original.clone();
    hash.hash.clone_from(&fixture.replacement.hash);
    let mut workflows = fixture.original.clone();
    workflows.workflows.remove("Example");
    let mut unknown = fixture.original.clone();
    unknown.id = typed_id::generate("dep");
    let mut malformed = fixture.original.clone();
    malformed.id = "invalid".into();
    let mut tx = fixture.journal.begin().await.unwrap();
    let foreign = app::active_deploy(&mut tx, &fixture.foreign).await.unwrap();
    tx.commit().await.unwrap();
    for expected in [
        hash,
        workflows,
        changed_schedule(&fixture.original),
        unknown,
        malformed,
        foreign,
    ] {
        assert!(matches!(
            fixture.apply_exact(&expected).await,
            Err(WorkflowServiceError::Unavailable(_))
        ));
        assert_eq!(fixture.snapshot().await, before);
    }
    fixture.apply_exact(&fixture.original).await.unwrap();
    fixture.assert_generation(1, &fixture.original.id).await;
    malformed_stored_identity(&fixture).await;

    let missing_workflow = fixture
        .deployments
        .publish(
            &fixture.owner,
            &DeployRegistration {
                id: typed_id::generate("dep"),
                hash: String::new(),
                workflows: ["Child".into()].into(),
                schedules: Vec::new(),
            },
            &Sources::default(),
        )
        .await
        .unwrap();
    fixture
        .service
        .retain_deploy(&fixture.owner, &missing_workflow)
        .await
        .unwrap();
    let before = fixture.snapshot().await;
    assert!(matches!(fixture.apply_exact(&missing_workflow).await,
        Err(WorkflowServiceError::Conflict(message)) if message.contains("workflow is absent")));
    assert_eq!(fixture.snapshot().await, before);
    fixture.apply_exact(&fixture.original).await.unwrap();
    fixture.assert_generation(2, &fixture.original.id).await;
}

async fn malformed_stored_identity(fixture: &Fixture) {
    let mut expected = fixture.original.clone();
    expected.id = "invalid-stored-deployment".into();
    expected.hash = "f".repeat(64);
    let mut document = fixture
        .row(
            "deploys",
            json!({"app_id":fixture.owner.as_str(),"id":fixture.original.id}),
        )
        .await
        .0;
    document["id"] = value!(expected.id);
    document["hash"] = value!(expected.hash);
    document["manifest"] = value!(serde_json::to_string(&expected).unwrap());
    document["active"] = value!(0);
    let tx = fixture.journal.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_deploys")
        .unwrap()
        .insert(document)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before = fixture.snapshot().await;
    assert!(matches!(
        fixture.apply_exact(&expected).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(fixture.snapshot().await, before);
}

fn metadata_damage(fixture: &Fixture) -> Vec<(&'static str, Value)> {
    let mut wrong_id = fixture.original.clone();
    wrong_id.id.clone_from(&fixture.replacement.id);
    let mut wrong_hash = fixture.original.clone();
    wrong_hash.hash.clone_from(&fixture.replacement.hash);
    let mut missing_workflow = fixture.original.clone();
    missing_workflow.workflows.remove("Example");
    vec![
        (
            "manifest identity",
            value!({"manifest":serde_json::to_string(&wrong_id).unwrap()}),
        ),
        (
            "manifest hash",
            value!({"manifest":serde_json::to_string(&wrong_hash).unwrap()}),
        ),
        (
            "workflow declaration",
            value!({"manifest":serde_json::to_string(&missing_workflow).unwrap()}),
        ),
        (
            "schedule declaration",
            value!({"manifest":serde_json::to_string(&changed_schedule(&fixture.original)).unwrap()}),
        ),
        ("undecodable manifest", value!({"manifest":"{"})),
        ("invalid hash", value!({"hash":"invalid"})),
        ("different valid hash", value!({"hash":"f".repeat(64)})),
        (
            "invalid availability epoch",
            value!({"availability_epoch":-1}),
        ),
        ("unavailable deployment", value!({"state":"unavailable"})),
        ("retiring deployment", value!({"state":"retiring"})),
    ]
}

fn hold_damage(fixture: &Fixture) -> Vec<(&'static str, Value)> {
    vec![
        ("releasing hold", value!({"state":"releasing"})),
        ("invalid generation", value!({"generation":0})),
        (
            "foreign holder",
            value!({"holder_id":HoldScope::for_app(fixture.foreign.clone()).holder()}),
        ),
        (
            "queue holder",
            value!({"holder_id":HoldScope::for_queue(fixture.owner.clone()).holder()}),
        ),
        (
            "different hash",
            value!({"deploy_hash":fixture.replacement.hash}),
        ),
        ("missing hash", value!({"deploy_hash":Value::Null})),
        (
            "missing scoped hold",
            value!({"app_id":fixture.foreign.as_str()}),
        ),
    ]
}

pub(super) async fn damaged_local(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let deployment = fixture
        .row(
            "deploys",
            json!({"app_id":fixture.owner.as_str(),"id":fixture.original.id}),
        )
        .await;
    let hold = fixture
        .row(
            "deployment_holds",
            json!({"app_id":fixture.owner.as_str(),"deploy_id":fixture.original.id}),
        )
        .await;
    let cases = metadata_damage(&fixture)
        .into_iter()
        .map(|(label, patch)| ("deploys", &deployment, label, patch))
        .chain(
            hold_damage(&fixture)
                .into_iter()
                .map(|(label, patch)| ("deployment_holds", &hold, label, patch)),
        );
    for (index, (table, original, label, changes)) in cases.enumerate() {
        fixture
            .patch(table, &original.text("id").unwrap(), changes)
            .await;
        let before = fixture.snapshot().await;
        let result = fixture.apply_exact(&fixture.original).await;
        assert!(
            matches!(
                result,
                Err(WorkflowServiceError::Unavailable(_)
                    | WorkflowServiceError::Internal(_)
                    | WorkflowServiceError::PermissionDenied)
            ),
            "{label} must remain a retryable infrastructure failure: {result:?}"
        );
        if table == "deploys" && label != "undecodable manifest" {
            assert!(matches!(result, Err(WorkflowServiceError::Unavailable(_))));
        }
        assert_eq!(fixture.snapshot().await, before, "{label}");
        fixture.restore(table, original.0.clone()).await;
        fixture.apply_exact(&fixture.original).await.unwrap();
        fixture
            .assert_generation(i64::try_from(index + 1).unwrap(), &fixture.original.id)
            .await;
    }
    assert_eq!(fixture.active().await, fixture.replacement);
}

pub(super) async fn ordinary_latest(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let deployment = fixture
        .row(
            "deploys",
            json!({"app_id":fixture.owner.as_str(),"id":fixture.replacement.id}),
        )
        .await;
    let hold = fixture
        .row(
            "deployment_holds",
            json!({"app_id":fixture.owner.as_str(),"deploy_id":fixture.replacement.id}),
        )
        .await;
    let cases = [
        ("deploys", &deployment, value!({"hash":"f".repeat(64)})),
        // The selected row must not redirect to another valid retained registration.
        (
            "deploys",
            &deployment,
            value!({"manifest":serde_json::to_string(&fixture.original).unwrap()}),
        ),
        ("deployment_holds", &hold, value!({"state":"releasing"})),
        (
            "deployment_holds",
            &hold,
            value!({"deploy_hash":fixture.original.hash}),
        ),
    ];
    for (index, (table, original, changes)) in cases.into_iter().enumerate() {
        fixture
            .patch(table, &original.text("id").unwrap(), changes)
            .await;
        let before = fixture.snapshot().await;
        let request = RequestId::mint();
        assert!(matches!(
            fixture
                .scope()
                .restart(&request, &fixture.run, RestartOptions::default())
                .await,
            Err(WorkflowServiceError::Unavailable(_))
        ));
        assert_eq!(fixture.snapshot().await, before);
        fixture.restore(table, original.0.clone()).await;
        let result = fixture
            .scope()
            .restart(&request, &fixture.run, RestartOptions::default())
            .await
            .unwrap();
        assert_eq!(result.pinned_to, fixture.replacement.id);
        fixture
            .assert_generation(i64::try_from(index + 1).unwrap(), &fixture.replacement.id)
            .await;
    }
}
