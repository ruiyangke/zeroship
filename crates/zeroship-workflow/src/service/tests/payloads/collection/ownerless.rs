use super::*;

/// A staged payload's `task_id` is its staging-lease owner, not its durable
/// one. Durable ownership is an edge in `payload_refs`, and the ownership
/// disjunction in `payloads::owned_reference` consults `task_id` only for the
/// `staged` half and only while the run still holds a task. So for a staged
/// payload that names no task, expiry is the whole eligibility test -- and
/// every staged row carries a NOT NULL `expires_at`, so that test is always
/// answerable.
///
/// One collection sweep over five payloads that differ in exactly two
/// variables, owner and expiry. What the sweep must delete and what it must
/// leave are asserted as an exact set, so truncation cannot pass as collection.
pub(super) async fn ownerless(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let output = reference(b"retained output");
    let referenced = fixture
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
        .unwrap()
        .id;
    let ownerless_expired = fixture.stage().await;
    let ownerless_live = fixture.stage().await;
    let owned_expired = fixture.stage().await;
    let owned_live = fixture.stage().await;

    // Promote one payload onto a durable reference edge while the task is still
    // live, because that promotion is the one path `task_id` gates.
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
    assert_eq!(fixture.payload(&referenced).await.state, "referenced");

    for id in [&referenced, &ownerless_expired, &ownerless_live] {
        fixture.disown(id).await;
    }
    for id in [&referenced, &ownerless_expired, &ownerless_live] {
        assert_eq!(
            fixture.payload(id).await.task_id,
            None,
            "{id} must carry no staging-lease owner"
        );
    }
    // The control half really is task-owned, so a sweep that ignored the owner
    // entirely would still have to answer for it.
    for id in [&owned_expired, &owned_live] {
        assert_eq!(
            fixture.payload(id).await.task_id.as_deref(),
            Some(fixture.task.id.as_str()),
            "{id} must still name the task that staged it"
        );
    }

    for id in [&referenced, &ownerless_expired, &owned_expired] {
        fixture.expire(id).await;
    }
    let now = fixture.now().await;
    for id in [&referenced, &ownerless_expired, &owned_expired] {
        assert!(
            fixture.payload(id).await.expires_at <= now,
            "{id} must be expired before the sweep"
        );
    }
    // And the ones held back are genuinely unexpired, which is what makes the
    // pair a control rather than two spellings of the same input.
    for id in [&ownerless_live, &owned_live] {
        assert!(
            fixture.payload(id).await.expires_at > now,
            "{id} must still be inside its staging window"
        );
    }

    assert_eq!(
        fixture
            .scope
            .collect_job(
                &Grant::new(fixture.scope.app_id()),
                options(8),
                &fixture.objects
            )
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );

    // Expired and unowned: reclaimed, leaving the tombstone a first deletion
    // records.
    assert_eq!(fixture.payload(&ownerless_expired).await.state, "deleted");
    // Expired and task-owned: exactly as before, which is the regression
    // control for the path this change did not touch.
    assert_eq!(fixture.payload(&owned_expired).await.state, "deleted");
    // Unexpired: untouched, owner or no owner. Expiry alone decides.
    assert_eq!(fixture.payload(&ownerless_live).await.state, "staged");
    assert_eq!(fixture.payload(&owned_live).await.state, "staged");
    // Referenced: held by its edge, and losing the task owner does not release
    // it -- durable ownership was never the task's.
    assert_eq!(fixture.payload(&referenced).await.state, "referenced");

    let app = fixture.scope.app_id();
    assert!(!fixture.exists(app, &ownerless_expired));
    assert!(!fixture.exists(app, &owned_expired));
    assert!(fixture.exists(app, &ownerless_live));
    assert!(fixture.exists(app, &owned_live));
    assert!(fixture.exists(app, &referenced));

    let mut deleted = fixture.objects.deletes();
    deleted.sort();
    let mut expected = vec![ownerless_expired, owned_expired];
    expected.sort();
    assert_eq!(
        deleted, expected,
        "the sweep must delete the expired unreferenced payloads and nothing else"
    );

    // The reference edge the referenced payload rests on survived the sweep.
    let tx = fixture.store.begin().await.unwrap();
    assert!(!journal_rows(
        &tx,
        "payload_refs",
        json!({"app_id":app.as_str(), "payload_id":referenced}),
    )
    .await
    .is_empty());
    tx.commit().await.unwrap();
}
