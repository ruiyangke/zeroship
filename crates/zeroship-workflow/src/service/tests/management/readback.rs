use super::*;

#[compio::test]
async fn postgres_management_receipt_read_waits_for_atomic_application() {
    let fixture = PostgresFixture::start().await;
    let (service, app_id, _, _deployments) =
        registered_service(Rc::new(fixture.store.clone())).await;
    let run = start(&service, &app_id).await;
    let scope = service.fixture_app(app_id.clone());
    let reader_store = orm_store(
        &fixture
            .admin_url
            .replacen("postgres@", "customer_worker@", 1),
        crate::service::store::SchemaName::new("customer").unwrap(),
    )
    .await;
    let reader = WorkflowService::open(Rc::new(reader_store), service.policies.clone())
        .await
        .unwrap()
        .fixture_app(app_id.clone());
    let grant = started(&app_id, &run, 1);
    let barrier = PgBarrier::install(&fixture.admin_url, BarrierSite::Receipt).await;
    let (worker, pending) = match select(
        barrier.blocked_worker().boxed_local(),
        scope.management_job(&grant).boxed_local(),
    )
    .await
    {
        Either::Left(pair) => pair,
        Either::Right((result, _)) => {
            panic!("application ended before final receipt write: {result:?}")
        }
    };
    let waiting_reader = async {
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let blocked = barrier.observer.query(
                    "SELECT pid FROM pg_stat_activity WHERE usename='customer_worker' AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid))",
                    &[&worker],
                ).await.unwrap();
                if !blocked.is_empty() { return; }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("receipt reader must wait for the applying app transaction");
    };
    let read = reader.job_receipt(&grant.delivery.job).boxed_local();
    let read = match select(waiting_reader.boxed_local(), read).await {
        Either::Left(((), read)) => read,
        Either::Right((result, _)) => {
            panic!("receipt read escaped applying transaction: {result:?}")
        }
    };
    barrier.release().await;
    let (applied, received) = futures::join!(pending, read);
    assert_eq!(received.unwrap(), Some(applied.unwrap()));
    barrier.remove_trigger().await;
    assert_eq!(head(&service, &app_id, &run).await, (1, "queued".into()));
}
