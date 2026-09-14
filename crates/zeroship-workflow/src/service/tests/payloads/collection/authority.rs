use super::*;

pub(super) async fn replacement(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let (entered, resume) = fixture.backend.gate(&id);
    let grant = Grant::new(fixture.scope.app_id());
    let replace = async {
        entered.recv_async().await.unwrap();
        let binding = fixture
            .service
            .policies
            .bind(fixture.scope.app_id().clone())
            .unwrap();
        binding
            .begin_refresh()
            .unwrap()
            .install(leased_policy(2, AppPolicy::default()))
            .unwrap();
    };
    let (result, ()) = futures::join!(fixture.scope.collect_job(&grant, options(1)), replace);
    assert!(
        matches!(result, Err(WorkflowServiceError::Unavailable(_))),
        "{result:?}"
    );
    assert!(
        resume.is_disconnected(),
        "policy cancellation drops storage I/O"
    );
    recover_reserved(&fixture, &grant, &id).await;
}

pub(super) async fn original_expiry(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let (entered, resume) = fixture.backend.gate(&id);
    let binding = fixture
        .service
        .policies
        .current_binding(fixture.scope.app_id())
        .unwrap();
    let original = Instant::now() + Duration::from_secs(2);
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), original).unwrap(),
        )
        .unwrap();
    let grant = Grant::new(fixture.scope.app_id());
    let extend = async {
        entered.recv_async().await.unwrap();
        binding
            .begin_refresh()
            .unwrap()
            .install(
                PolicySnapshot::lease(
                    2.try_into().unwrap(),
                    AppPolicy::default(),
                    original + Duration::from_secs(30),
                )
                .unwrap(),
            )
            .unwrap();
    };
    let (result, ()) = futures::join!(fixture.scope.collect_job(&grant, options(1)), extend);
    assert!(
        matches!(result, Err(WorkflowServiceError::Unavailable(_))),
        "{result:?}"
    );
    binding.authority().unwrap().check().unwrap();
    assert!(resume.is_disconnected());
    recover_reserved(&fixture, &grant, &id).await;
}

pub(super) async fn delivery_expiry(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let (entered, resume) = fixture.backend.gate(&id);
    let mut grant = Grant::new(fixture.scope.app_id());
    grant.expires = Instant::now() + Duration::from_secs(2);
    let wait = async {
        entered.recv_async().await.unwrap();
    };
    let (result, ()) = futures::join!(fixture.scope.collect_job(&grant, options(1)), wait);
    assert!(
        matches!(result, Err(WorkflowServiceError::Timeout)),
        "{result:?}"
    );
    fixture
        .service
        .policies
        .current_binding(fixture.scope.app_id())
        .unwrap()
        .authority()
        .unwrap()
        .check()
        .unwrap();
    assert!(resume.is_disconnected());
    recover_reserved(&fixture, &grant, &id).await;
}

async fn recover_reserved(fixture: &Fixture, grant: &Grant, id: &str) {
    assert_eq!(fixture.payload(id).await.state, "deleting");
    assert_eq!(fixture.payload(id).await.expires_at, 0);
    assert!(fixture.exists(fixture.scope.app_id(), id).await);
    let page = fixture.page(&grant.delivery.job).await;
    assert_eq!(page.next_index, 1);
    assert!(fixture
        .scope
        .job_receipt(&grant.delivery.job)
        .await
        .unwrap()
        .is_none());
    let current = fixture.reopen(true).await;
    assert_eq!(
        current
            .collect_job(&grant.retry(), options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(fixture.backend.calls(), [id]);
    assert_eq!(fixture.page(&grant.delivery.job).await, page);
    current
        .collect_job(&Grant::new(fixture.scope.app_id()), options(1))
        .await
        .unwrap();
    assert_eq!(fixture.payload(id).await.state, "deleted");
    assert!(!fixture.exists(fixture.scope.app_id(), id).await);
    assert_eq!(fixture.backend.calls(), [id, id]);
}

pub(super) async fn stale_confirmation(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let (entered, resume) = fixture.backend.gate(&id);
    let old = Grant::new(fixture.scope.app_id());
    let newer = Grant::new(fixture.scope.app_id());
    let other_host = fixture.reopen(true).await;
    let concurrent = async {
        entered.recv_async().await.unwrap();
        let receipt = other_host.collect_job(&newer, options(1)).await.unwrap();
        let tombstone = fixture.payload(&id).await;
        let scan = fixture.scan().await;
        resume.send_async(()).await.unwrap();
        (receipt, tombstone, scan)
    };
    let (old_receipt, (new_receipt, tombstone, scan)) =
        futures::join!(fixture.scope.collect_job(&old, options(1)), concurrent);
    assert_eq!(old_receipt.unwrap().outcome, JobOutcome::Completed {});
    assert_eq!(new_receipt.outcome, JobOutcome::Completed {});
    assert_eq!(tombstone.state, "deleted");
    assert!(tombstone.expires_at > 0);
    assert_eq!(fixture.payload(&id).await, tombstone);
    assert_eq!(fixture.scan().await, scan);
    assert_eq!(fixture.backend.calls(), [id.clone(), id]);
}
