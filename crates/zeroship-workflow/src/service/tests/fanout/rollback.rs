use super::*;

enum Database {
    Sqlite(std::path::PathBuf),
    Postgres(Box<compio_postgres::Client>),
}
impl Database {
    async fn fault(&self, table: &str, enabled: bool) {
        match self {
            Self::Sqlite(path) => {
                let path = path.clone();
                let sql = if enabled { format!("CREATE TRIGGER fail_fanout BEFORE INSERT ON __zeroship_workflow_{table} BEGIN SELECT RAISE(ABORT, 'injected fanout failure'); END") } else { "DROP TRIGGER fail_fanout".into() };
                compio::runtime::spawn_blocking(move || rusqlite::Connection::open(path).unwrap().execute_batch(&sql).unwrap()).await.unwrap();
            }
            Self::Postgres(client) => client.batch_execute(&if enabled {
                format!("CREATE FUNCTION customer.fail_fanout() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected fanout failure'; END $$; CREATE TRIGGER fail_fanout BEFORE INSERT ON customer.__zeroship_workflow_{table} FOR EACH ROW EXECUTE FUNCTION customer.fail_fanout();")
            } else { format!("DROP TRIGGER fail_fanout ON customer.__zeroship_workflow_{table}; DROP FUNCTION customer.fail_fanout();") }).await.unwrap(),
        }
    }
}

#[compio::test]
async fn sqlite_fanout_final_page_or_successor_failure_rolls_back_signals() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    let store = Rc::new(sqlite_store(&path).await);
    Box::pin(rollback(store, Database::Sqlite(path))).await;
}
#[compio::test]
async fn postgres_fanout_final_page_or_successor_failure_rolls_back_signals() {
    let fixture = PostgresFixture::start().await;
    let client = connect(&fixture.admin_url).await;
    Box::pin(rollback(
        Rc::new(fixture.store.clone()),
        Database::Postgres(Box::new(client)),
    ))
    .await;
}

async fn rollback(store: Rc<OrmStore>, database: Database) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app);
    let worker = WorkerIdentity::new("fanout-rollback".into()).unwrap();
    wait_on_topic(&service, &scope, &worker).await;
    let accepted = broadcast(&scope, "transactional").await;
    let grant = Grant::new(&job(&scope, &accepted.id, 1).await);
    let before = snapshot(&scope).await;
    for table in ["job_publications", "fanout_pages"] {
        database.fault(table, true).await;
        assert!(scope
            .fanout_job(&grant, FanoutOptions::default())
            .await
            .is_err());
        assert_eq!(snapshot(&scope).await, before);
        assert!(scope
            .job_receipt(&grant.delivery.job)
            .await
            .unwrap()
            .is_none());
        database.fault(table, false).await;
    }
    let receipt = scope
        .fanout_job(&grant.retry(), FanoutOptions::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    assert_eq!(count_signals(&scope).await, 1);
    let after = snapshot(&scope).await;
    assert_eq!(
        scope
            .fanout_job(&grant.retry(), FanoutOptions::default())
            .await
            .unwrap(),
        Some(receipt)
    );
    assert_eq!(snapshot(&scope).await, after);
}
