use super::*;
use crate::service::models::{app_state, broadcasts, fanout_pages, fanout_publications, topics};

pub(super) async fn damage(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let worker = WorkerIdentity::new("fanout-damage".into()).unwrap();
    wait_on_topic(&service, &scope, &worker).await;
    wait_on_topic(&service, &scope, &worker).await;
    let first = broadcast(&scope, "first").await;
    let second = broadcast(&scope, "second").await;
    let first_job = job(&scope, &first.id, 1).await;
    let second_job = job(&scope, &second.id, 1).await;
    set_cursor(&service, &first.id, 1).await;
    refused(&scope, &first_job).await;
    set_cursor(&service, &first.id, 0).await;
    set_completed(&service, &app, 1).await;
    refused(&scope, &second_job).await;
    set_completed(&service, &app, 0).await;
    Box::pin(damaged_publication(&service, &scope, &first_job)).await;
    let receipt = scope
        .fanout_job(&Grant::new(&first_job), FanoutOptions { page_size: 1 })
        .await
        .unwrap()
        .unwrap();
    let next = job(&scope, &first.id, 2).await;
    let tx = service.begin().await.unwrap();
    let page = journal_rows(&tx, "fanout_pages", json!({"id":first_job.id.as_str()})).await;
    let original = page[0].text("result").unwrap();
    tx.commit().await.unwrap();
    let result: serde_json::Value = serde_json::from_str(&original).unwrap();
    set_cursor(&service, &first.id, result["after"].as_i64().unwrap() + 1).await;
    refused(&scope, &next).await;
    set_cursor(&service, &first.id, result["after"].as_i64().unwrap()).await;
    let mut damaged = result;
    damaged["unknown"] = json!(true);
    let tx = service.begin().await.unwrap();
    tx.database()
        .entity::<fanout_pages::Entity>()
        .unwrap()
        .update_many(
            fanout_pages::id.eq(first_job.id.as_str()).unwrap(),
            fanout_pages::result.set(damaged.to_string()).unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    refused(&scope, &next).await;
    assert!(scope.job_receipt(&first_job).await.is_err());
    let tx = service.begin().await.unwrap();
    tx.database()
        .entity::<fanout_pages::Entity>()
        .unwrap()
        .update_many(
            fanout_pages::id.eq(first_job.id.as_str()).unwrap(),
            fanout_pages::result.set(original).unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    scope
        .fanout_job(&Grant::new(&next), FanoutOptions::default())
        .await
        .unwrap()
        .unwrap();
    set_completed(&service, &app, 0).await;
    refused(&scope, &second_job).await;
    set_completed(&service, &app, 1).await;
    scope
        .fanout_job(&Grant::new(&second_job), FanoutOptions::default())
        .await
        .unwrap()
        .unwrap();
    // Old page replay does not require being today's topic head.
    assert_eq!(
        scope
            .fanout_job(&Grant::new(&first_job), FanoutOptions::default())
            .await
            .unwrap(),
        Some(receipt)
    );
}

async fn damaged_publication(service: &WorkflowService, scope: &AppWorkflows, job: &JobSpec) {
    for revision in [2_i64, 1] {
        let tx = service.begin().await.unwrap();
        tx.database()
            .entity::<fanout_publications::Entity>()
            .unwrap()
            .update_many(
                fanout_publications::id.eq(job.id.as_str()).unwrap(),
                fanout_publications::revision.set(revision).unwrap(),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        if revision != 1 {
            refused(scope, job).await;
            assert!(scope.pending_jobs(None, 100).await.is_err());
        }
    }
}

async fn refused(scope: &AppWorkflows, job: &JobSpec) {
    let before = snapshot(scope).await;
    assert!(scope
        .fanout_job(&Grant::new(job), FanoutOptions::default())
        .await
        .is_err());
    assert_eq!(snapshot(scope).await, before);
}
async fn set_cursor(service: &WorkflowService, id: &str, cursor: i64) {
    let tx = service.begin().await.unwrap();
    assert_eq!(
        tx.database()
            .entity::<broadcasts::Entity>()
            .unwrap()
            .update_many(
                broadcasts::id.eq(id).unwrap(),
                broadcasts::cursor.set(cursor).unwrap()
            )
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
}
async fn set_completed(service: &WorkflowService, app: &AppId, completed: i64) {
    let tx = service.begin().await.unwrap();
    assert_eq!(
        tx.database()
            .entity::<topics::Entity>()
            .unwrap()
            .update_many(
                topics::app_id.eq(app.as_str()).unwrap(),
                topics::completed_sequence.set(completed).unwrap()
            )
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
}

pub(super) async fn expiry(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let accepted = broadcast(&scope, "waiting for lock").await;
    let grant = Grant::new(&job(&scope, &accepted.id, 1).await);
    let before = snapshot(&scope).await;
    let mut blocker = service.begin().await.unwrap();
    crate::service::app::lock_app_state(&mut blocker, &app)
        .await
        .unwrap();
    let binding = service.policies.current_binding(&app).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), deadline).unwrap(),
        )
        .unwrap();
    let mut pending = Box::pin(scope.fanout_job(&grant, FanoutOptions::default()));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(
                2.try_into().unwrap(),
                AppPolicy::default(),
                deadline + Duration::from_secs(30),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        compio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap(),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    binding.authority().unwrap().check().unwrap();
    blocker.commit().await.unwrap();
    assert_eq!(snapshot(&scope).await, before);
    assert!(scope
        .fanout_job(&grant.retry(), FanoutOptions::default())
        .await
        .unwrap()
        .is_some());
}

pub(super) async fn counters(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let worker = WorkerIdentity::new("fanout-overflow".into()).unwrap();
    wait_on_topic(&service, &scope, &worker).await;
    let accepted = broadcast(&scope, "overflow").await;
    let grant = Grant::new(&job(&scope, &accepted.id, 1).await);
    let tx = service.begin().await.unwrap();
    tx.database()
        .entity::<app_state::Entity>()
        .unwrap()
        .update_many(
            app_state::app_id.eq(app.as_str()).unwrap(),
            app_state::signal_sequence.set(i64::MAX).unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before = snapshot(&scope).await;
    assert!(matches!(
        scope.fanout_job(&grant, FanoutOptions::default()).await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    assert_eq!(snapshot(&scope).await, before);
    let tx = service.begin().await.unwrap();
    tx.database()
        .entity::<app_state::Entity>()
        .unwrap()
        .update_many(
            app_state::app_id.eq(app.as_str()).unwrap(),
            app_state::signal_sequence.set(0_i64).unwrap(),
        )
        .await
        .unwrap();
    tx.database()
        .entity::<topics::Entity>()
        .unwrap()
        .update_many(
            topics::app_id.eq(app.as_str()).unwrap(),
            topics::accepted_sequence.set(i64::MAX).unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before = snapshot(&scope).await;
    assert!(matches!(
        scope
            .broadcast(
                &RequestId::mint(),
                "updates",
                SignalOptions {
                    signal_type: "news".into(),
                    payload: json!(null)
                }
            )
            .await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    assert_eq!(snapshot(&scope).await, before);
    let tx = service.begin().await.unwrap();
    tx.database()
        .entity::<topics::Entity>()
        .unwrap()
        .update_many(
            topics::app_id.eq(app.as_str()).unwrap(),
            topics::accepted_sequence.set(1_i64).unwrap(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(scope
        .fanout_job(&grant.retry(), FanoutOptions::default())
        .await
        .unwrap()
        .is_some());
}
