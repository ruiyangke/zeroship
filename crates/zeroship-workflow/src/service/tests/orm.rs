use super::objects::{Objects, StepOutputs};
use super::*;
use crate::{backend::WorkflowBackend, service::WorkerIdentity};
use futures::{channel::oneshot, FutureExt};
use std::{task::Poll, time::Duration};

#[compio::test]
async fn sqlite_independent_orm_hosts_serialize_admission_and_claims() {
    let directory = tempfile::tempdir().unwrap();
    let path = journal_file(directory.path());
    let first = sqlite_store(&path).await;
    let second = sqlite_store(&path).await;
    independent_hosts(first, second).await;
}

#[compio::test]
async fn postgres_independent_orm_hosts_serialize_admission_and_claims() {
    let fixture = PostgresFixture::start().await;
    let second = orm_store(&fixture.journal_url)
    .await;
    independent_hosts(fixture.store.clone(), second).await;
}

async fn independent_hosts(first: OrmStore, second: OrmStore) {
    let (first, app, _, _deployments) = registered_service(Rc::new(first)).await;
    let second = WorkflowService::open(Rc::new(second), first.policies.clone())
        .await
        .unwrap()
        .with_deployments(first.deployments.clone().unwrap());
    let first_app = first.fixture_app(app.clone());
    let second_app = second.fixture_app(app);
    let request = RequestId::mint();
    let options = StartOptions {
        key: Some("invoice".into()),
        ..Default::default()
    };
    let (a, b) = futures::join!(
        first_app.start(&request, "Example", options.clone()),
        second_app.start(&request, "Example", options),
    );
    let started = a.unwrap();
    assert_eq!(started, b.unwrap());
    let first_worker = WorkerIdentity::new("first-host".into()).unwrap();
    let second_worker = WorkerIdentity::new("second-host".into()).unwrap();
    let (a, b) = futures::join!(first.poll(&first_worker), second.poll(&second_worker));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(
        a.is_some(),
        b.is_some(),
        "a live task has only one claimant"
    );
    assert_eq!(a.or(b).unwrap().invocation.run_id, started.id);
}

#[compio::test]
async fn native_client_crosses_runtime_threads_without_losing_app_scope() {
    let directory = tempfile::tempdir().unwrap();
    let store = Rc::new(sqlite_store(&journal_file(directory.path())).await);
    let (service, app, other, _deployments) = registered_service(store).await;
    let objects = Objects::new();
    let client = service
        .fixture_app(app.clone())
        .into_backend(&service, StepOutputs::shared(&objects, 1024), objects.stager())
        .unwrap();
    let other_client = service
        .fixture_app(other)
        .into_backend(&service, StepOutputs::shared(&objects, 1024), objects.stager())
        .unwrap();
    let (reply, result) = oneshot::channel();
    let thread = std::thread::spawn(move || {
        let runtime = compio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let run = client
                .start("Example".into(), serde_json::Value::Null, StartOptions::default())
                .await
                .unwrap();
            assert_eq!(
                client.status(run.id.clone()).await.unwrap().state,
                run.state
            );
            assert!(matches!(
                other_client.status(run.id.clone()).await,
                Err(WorkflowServiceError::NotFound(_))
            ));
            reply.send(run).unwrap();
        });
    });
    let run = compio::time::timeout(Duration::from_secs(10), result)
        .await
        .unwrap()
        .unwrap();
    thread.join().unwrap();
    assert_eq!(
        service
            .fixture_app(app)
            .status(&run.id)
            .await
            .unwrap()
            .state,
        run.state
    );
}

#[compio::test]
async fn mutating_calls_hint_the_host_and_reads_do_not() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let directory = tempfile::tempdir().unwrap();
    let store = Rc::new(sqlite_store(&journal_file(directory.path())).await);
    let (service, app, _, _deployments) = registered_service(store).await;
    let hints = Arc::new(AtomicUsize::new(0));
    let observed = hints.clone();
    let objects = Objects::new();
    let api = service
        .fixture_app(app)
        .with_publication_hint(Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
    let client = api
        .clone()
        .into_backend(&service, StepOutputs::shared(&objects, 1024), objects.stager())
        .unwrap();
    let run = client
        .start("Example".into(), serde_json::Value::Null, StartOptions::default())
        .await
        .unwrap();
    assert_eq!(hints.load(Ordering::SeqCst), 1);
    client.status(run.id.clone()).await.unwrap();
    assert_eq!(hints.load(Ordering::SeqCst), 1, "reads commit nothing");
    // A boundary refusal reaches no commit, so it must not wake the host. The
    // run id is well-formed and absent, so the refusal comes from the journal
    // before the transaction commits.
    assert!(api
        .signal(
            &RequestId::mint(),
            zeroship_core::workflow_coordination::RunId::mint().as_str(),
            SignalOptions {
                signal_type: "resume".into(),
                payload: json!(null),
            },
        )
        .await
        .is_err());
    assert_eq!(
        hints.load(Ordering::SeqCst),
        1,
        "a boundary refusal commits nothing"
    );
    // A malformed workflow name is refused before a transaction opens, so it
    // cannot wake the host either.
    assert!(api
        .start(&RequestId::mint(), "", StartOptions::default())
        .await
        .is_err());
    assert_eq!(
        hints.load(Ordering::SeqCst),
        1,
        "a boundary refusal commits nothing"
    );
    // The hint lives on the handle, so a clone shares it.
    api.clone()
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    assert_eq!(hints.load(Ordering::SeqCst), 2, "clones share the hint");
    // A handle with no hint commits without telling the host.
    let plain = service.fixture_app(api.app_id().clone());
    plain
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    assert_eq!(hints.load(Ordering::SeqCst), 2);
}

/// An acceptance refused before its transaction commits leaves no intent, so it
/// must not wake the host. A transition, a restart and a redeemed token all
/// share the one acceptance path, and each refusal below stops before commit.
#[compio::test]
async fn refusals_before_commit_do_not_hint_the_host() {
    use crate::operations::{RestartOptions, RunOperation};
    use crate::service::{capability::SignalTarget, SignalAuthority};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zeroship_core::service_assertion::{ServiceSigningKey, ServiceTrustBundle};
    let directory = tempfile::tempdir().unwrap();
    let store = Rc::new(sqlite_store(&journal_file(directory.path())).await);
    let (service, app, _, _deployments) = registered_service(store).await;
    let service = service.with_signal_authority(Arc::new(
        SignalAuthority::new(
            Arc::new(ServiceSigningKey::generate()),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
    ));
    let hints = Arc::new(AtomicUsize::new(0));
    let observed = hints.clone();
    let scope = service
        .fixture_app(app)
        .with_publication_hint(Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
    // A well-formed but absent run reaches the journal and is refused before
    // its transaction commits.
    assert!(scope
        .transition(
            &RequestId::mint(),
            zeroship_core::workflow_coordination::RunId::mint().as_str(),
            RunOperation::Pause,
        )
        .await
        .is_err());
    assert!(scope
        .restart(
            &RequestId::mint(),
            zeroship_core::workflow_coordination::RunId::mint().as_str(),
            RestartOptions::default(),
        )
        .await
        .is_err());
    // A token that fails capability verification is refused before its
    // transaction commits.
    let target = SignalTarget::Run {
        run_id: typed_id::new_workflow_run_id(),
    };
    assert!(scope
        .ingest_signal(
            &RequestId::mint(),
            "not-a-token",
            &target,
            SignalOptions {
                signal_type: "resume".into(),
                payload: json!(null),
            },
        )
        .await
        .is_err());
    assert_eq!(
        hints.load(Ordering::SeqCst),
        0,
        "a boundary refusal commits nothing"
    );
    // The commit point still wakes the host: a successful start fires once.
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    assert_eq!(
        hints.load(Ordering::SeqCst),
        1,
        "a committed start wakes the host"
    );
}

#[compio::test]
async fn cancelled_queued_requests_do_not_mutate_and_overload_is_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let store = Rc::new(sqlite_store(&journal_file(directory.path())).await);
    let (service, app, _, _deployments) = registered_service(store.clone()).await;
    let objects = Objects::new();
    let client = service
        .fixture_app(app)
        .into_backend(&service, StepOutputs::shared(&objects, 1024), objects.stager())
        .unwrap();
    let mut waiting = Vec::new();
    // Do not yield to the engine until the request queue rejects admission.
    loop {
        assert!(
            waiting.len() < 1024,
            "request queue must apply backpressure"
        );
        let mut start = client
            .start("Example".into(), serde_json::Value::Null, StartOptions::default())
            .boxed_local();
        match futures::poll!(&mut start) {
            Poll::Pending => waiting.push(start),
            Poll::Ready(Err(WorkflowServiceError::ResourceExhausted(_))) => break,
            outcome => panic!("unexpected queued start result: {outcome:?}"),
        }
    }
    assert!(!waiting.is_empty());
    drop(waiting);
    let run = compio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client
                .start("Example".into(), serde_json::Value::Null, StartOptions::default())
                .await
            {
                Err(WorkflowServiceError::ResourceExhausted(_)) => {
                    compio::time::sleep(Duration::from_millis(1)).await
                }
                outcome => break outcome.unwrap(),
            }
        }
    })
    .await
    .unwrap();
    let tx = store.begin().await.unwrap();
    let rows = journal_rows(&tx, "runs", json!({})).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text("id").unwrap(), run.id);
}

#[compio::test]
async fn cancelling_a_database_wait_rolls_back_before_the_next_request() {
    let fixture = PostgresFixture::start().await;
    let (service, app, _, _deployments) = registered_service(Rc::new(fixture.store.clone())).await;
    let objects = Objects::new();
    let client = service
        .fixture_app(app.clone())
        .into_backend(&service, StepOutputs::shared(&objects, 1024), objects.stager())
        .unwrap();
    let blocker = connect(&fixture.admin_url).await;
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker
        .query_one(
            "SELECT app_id FROM workflow_manager.__zeroship_workflow_app_state WHERE app_id=$1 FOR UPDATE",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    let mut starting = client
        .start("Example".into(), serde_json::Value::Null, StartOptions::default())
        .boxed_local();
    assert!(futures::poll!(&mut starting).is_pending());
    let observer = connect(&fixture.admin_url).await;
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = observer.query_one("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE usename='zeroship_workflow' AND wait_event_type='Lock')", &[]).await.unwrap().get(0);
            if waiting { break; }
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.expect("request entered an ORM transaction and waited for the app lock");
    drop(starting);
    blocker.batch_execute("COMMIT").await.unwrap();
    let run = compio::time::timeout(
        Duration::from_secs(10),
        client.start("Example".into(), serde_json::Value::Null, StartOptions::default()),
    )
    .await
    .unwrap()
    .unwrap();
    let rows = observer
        .query("SELECT id FROM workflow_manager.__zeroship_workflow_runs", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), run.id);
}

#[compio::test]
async fn sqlite_journal_transaction_lifetime_rejects_abandoned_and_escaped_work() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&journal_file(directory.path())).await;
    transaction_lifetime(&store).await;
}

#[compio::test]
async fn postgres_journal_transaction_lifetime_rejects_abandoned_and_escaped_work() {
    let fixture = PostgresFixture::start().await;
    transaction_lifetime(&fixture.store).await;
}

async fn transaction_lifetime(store: &OrmStore) {
    for cancel_commit in [false, true] {
        let tx = store.begin().await.unwrap();
        journal_insert(
            &tx,
            "app_state",
            json!({
                "id":storage_id(), "app_id":"abandoned", "signal_epoch":0,
            }),
        )
        .await
        .unwrap();
        let escaped = tx.database().clone();
        let collection = escaped.collection("__zeroship_workflow_app_state").unwrap();
        let prepared = collection.insert(
            json!({
                "id":storage_id(), "app_id":"escaped", "signal_epoch":0,
            })
            .into(),
        );
        if cancel_commit {
            // Cancelling before settlement intent is sent must roll back.
            drop(tx.commit());
        } else {
            drop(tx);
        }
        let next = compio::time::timeout(Duration::from_secs(10), store.begin())
            .await
            .expect("abandoned transaction releases its database lease")
            .unwrap();
        assert_eq!(journal_count(&next, "app_state", json!({})).await, 0);
        assert!(prepared.await.is_err());
        assert!(collection
            .find(json!({}).into(), json!({}).into())
            .await
            .is_err());
        next.commit().await.unwrap();
    }

    let tx = store.begin().await.unwrap();
    journal_insert(
        &tx,
        "app_state",
        json!({
            "id":storage_id(), "app_id":"committed", "signal_epoch":0,
        }),
    )
    .await
    .unwrap();
    let escaped = tx.database().clone();
    tx.commit().await.unwrap();
    // A handle that outlived its transaction is refused at the collection it
    // would read through, so no read is reachable from it at all.
    assert!(escaped.collection("__zeroship_workflow_app_state").is_err());
    let tx = store.begin().await.unwrap();
    assert_eq!(
        journal_count(&tx, "app_state", json!({"app_id":"committed"})).await,
        1
    );
    tx.commit().await.unwrap();

    let held = store.begin().await.unwrap();
    let mut opening = store.begin().boxed_local();
    assert!(futures::poll!(&mut opening).is_pending());
    compio::time::sleep(Duration::from_millis(1)).await;
    assert!(futures::poll!(&mut opening).is_pending());
    drop(opening);
    let mut next = store.begin().boxed_local();
    assert!(futures::poll!(&mut next).is_pending());
    held.commit().await.unwrap();
    let next = compio::time::timeout(Duration::from_secs(10), next)
        .await
        .expect("cancelled begin must not orphan transaction admission")
        .unwrap();
    assert_eq!(journal_count(&next, "app_state", json!({})).await, 1);
    next.commit().await.unwrap();
}
