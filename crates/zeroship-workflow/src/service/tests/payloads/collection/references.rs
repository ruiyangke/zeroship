use super::*;
use crate::service::models::payloads;

pub(super) async fn retained(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let output = reference(b"retained output");
    let retained = fixture
        .service
        .stage_payload(
            &fixture.worker,
            &fixture.task.id,
            &fixture.task.token,
            &RequestId::mint(),
            output.clone(),
            fixture.objects.upload(b"retained output"),
        )
        .await
        .unwrap();
    let abandoned = fixture.stage().await;
    fixture
        .service
        .complete(
            &fixture.worker,
            &fixture.task.id,
            &fixture.task.token,
            execution(json!([{"kind":"RunCompleted", "outputRef":output}])),
        )
        .await
        .unwrap();
    fixture.expire(&retained.id).await;
    fixture.expire(&abandoned).await;
    let before = preserved(&fixture).await;
    let binding = fixture
        .service
        .policies
        .current_binding(fixture.scope.app_id())
        .unwrap();
    binding
        .begin_refresh()
        .unwrap()
        .install(leased_policy(
            2,
            AppPolicy {
                admission: false,
                dispatch: false,
                ingress: false,
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(
        fixture
            .scope
            .collect_job(
                &Grant::new(fixture.scope.app_id()),
                options(2),
                &fixture.objects
            )
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(fixture.payload(&retained.id).await.state, "referenced");
    assert!(fixture.exists(fixture.scope.app_id(), &retained.id));
    assert!(!fixture.exists(fixture.scope.app_id(), &abandoned));
    assert_eq!(preserved(&fixture).await, before);
    // A damaged eligibility projection cannot override actual reference edges.
    set_state(&fixture, &retained.id, "staged").await;
    fixture
        .scope
        .collect_job(
            &Grant::new(fixture.scope.app_id()),
            options(2),
            &fixture.objects,
        )
        .await
        .unwrap();
    assert_eq!(fixture.objects.deletes(), [abandoned]);
    assert_eq!(fixture.payload(&retained.id).await.state, "staged");
    assert_eq!(preserved(&fixture).await, before);
    set_state(&fixture, &retained.id, "referenced").await;
    let read = fixture
        .scope
        .read_payload(
            &fixture.task.invocation.run_id,
            fixture.task.generation,
            PayloadSlot::Output,
            fixture.objects.open(),
        )
        .await
        .unwrap();
    assert_eq!(read, b"retained output");
}

async fn preserved(fixture: &Fixture) -> Vec<Vec<zeroship_data_orm::Value>> {
    let tx = fixture.store.begin().await.unwrap();
    let mut snapshot = Vec::new();
    for table in [
        "runs",
        "generations",
        "tasks",
        "payload_refs",
        "requests",
        "deployment_holds",
    ] {
        let rows = journal_rows(
            &tx,
            table,
            json!({"app_id":fixture.scope.app_id().as_str()}),
        )
        .await;
        assert!(!rows.is_empty(), "fixture must retain {table}");
        snapshot.push(rows.into_iter().map(|row| row.0).collect());
    }
    tx.commit().await.unwrap();
    snapshot
}

async fn set_state(fixture: &Fixture, id: &str, state: &str) {
    let tx = fixture.store.begin().await.unwrap();
    assert_eq!(
        tx.database()
            .entity::<payloads::Entity>()
            .unwrap()
            .update_many(
                payloads::id.eq(id).unwrap(),
                payloads::state.set(state).unwrap()
            )
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
}

pub(super) async fn late_upload(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    fixture
        .scope
        .collect_job(
            &Grant::new(fixture.scope.app_id()),
            options(1),
            &fixture.objects,
        )
        .await
        .unwrap();
    let tombstone = fixture.payload(&id).await;
    assert_eq!(tombstone.state, "deleted");
    fixture
        .objects
        .put(fixture.scope.app_id(), &id, b"late upload");
    assert!(fixture.exists(fixture.scope.app_id(), &id));
    assert!(fixture
        .service
        .complete(
            &fixture.worker,
            &fixture.task.id,
            &fixture.task.token,
            execution(json!([{"kind":"RunCompleted", "outputRef":reference(b"collect-me")}]))
        )
        .await
        .is_err());
    fixture
        .scope
        .collect_job(
            &Grant::new(fixture.scope.app_id()),
            options(1),
            &fixture.objects,
        )
        .await
        .unwrap();
    assert_eq!(fixture.payload(&id).await, tombstone);
    assert!(fixture.exists(fixture.scope.app_id(), &id));
    fixture.expire(&id).await;
    fixture
        .scope
        .collect_job(
            &Grant::new(fixture.scope.app_id()),
            options(1),
            &fixture.objects,
        )
        .await
        .unwrap();
    // The resweep after the window makes the tombstone final.
    let purged = fixture.payload(&id).await;
    assert_eq!(purged.state, "purged");
    assert!(!fixture.exists(fixture.scope.app_id(), &id));
    assert_eq!(fixture.objects.deletes(), [id.clone(), id.clone()]);
    fixture.expire(&id).await;
    fixture
        .scope
        .collect_job(
            &Grant::new(fixture.scope.app_id()),
            options(1),
            &fixture.objects,
        )
        .await
        .unwrap();
    assert_eq!(fixture.payload(&id).await.state, "purged");
    assert_eq!(
        fixture.objects.deletes().len(),
        2,
        "a final tombstone is never swept"
    );
}
