use super::*;

enum Fault {
    Sqlite(std::path::PathBuf),
    Postgres(Box<compio_postgres::Client>),
}
impl Fault {
    async fn set(&self, enabled: bool) {
        match self {
            Self::Sqlite(path) => {
                let path = path.clone();
                let sql = if enabled {
                    "CREATE TRIGGER fail_collection BEFORE UPDATE OF outcome ON __zeroship_workflow_job_receipts BEGIN SELECT RAISE(ABORT, 'injected collection receipt failure'); END"
                } else { "DROP TRIGGER fail_collection" };
                compio::runtime::spawn_blocking(move || {
                    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap().execute_batch(sql).unwrap();
                }).await.unwrap();
            }
            Self::Postgres(client) => client.batch_execute(if enabled {
                "CREATE FUNCTION customer.fail_collection() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected collection receipt failure'; END $$; CREATE TRIGGER fail_collection BEFORE UPDATE OF outcome ON customer.__zeroship_workflow_job_receipts FOR EACH ROW EXECUTE FUNCTION customer.fail_collection();"
            } else { "DROP TRIGGER fail_collection ON customer.__zeroship_workflow_job_receipts; DROP FUNCTION customer.fail_collection();" }).await.unwrap(),
        }
    }
    async fn confirmation(&self, enabled: bool) {
        match self {
            Self::Sqlite(path) => {
                let path = path.clone();
                let sql = if enabled {
                    "CREATE TRIGGER fail_collection_confirmation BEFORE UPDATE OF state ON __zeroship_workflow_payloads WHEN NEW.state='deleted' BEGIN SELECT RAISE(ABORT, 'injected collection confirmation failure'); END"
                } else { "DROP TRIGGER fail_collection_confirmation" };
                compio::runtime::spawn_blocking(move || {
                    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap().execute_batch(sql).unwrap();
                }).await.unwrap();
            }
            Self::Postgres(client) => client.batch_execute(if enabled {
                "CREATE FUNCTION customer.fail_collection_confirmation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.state='deleted' THEN RAISE EXCEPTION 'injected collection confirmation failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_collection_confirmation BEFORE UPDATE OF state ON customer.__zeroship_workflow_payloads FOR EACH ROW EXECUTE FUNCTION customer.fail_collection_confirmation();"
            } else { "DROP TRIGGER fail_collection_confirmation ON customer.__zeroship_workflow_payloads; DROP FUNCTION customer.fail_collection_confirmation();" }).await.unwrap(),
        }
    }
}

#[compio::test]
async fn sqlite_collect_receipt_failure_rolls_back_scan_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    let store = Rc::new(sqlite_store(&path).await);
    Box::pin(rollback(store, Fault::Sqlite(path))).await;
}
#[compio::test]
async fn postgres_collect_receipt_failure_rolls_back_scan_only() {
    let database = PostgresFixture::start().await;
    let client = connect(&database.admin_url).await;
    Box::pin(rollback(
        Rc::new(database.store.clone()),
        Fault::Postgres(Box::new(client)),
    ))
    .await;
}

#[compio::test]
async fn sqlite_collect_confirmation_failure_leaves_recoverable_delete() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    let store = Rc::new(sqlite_store(&path).await);
    Box::pin(uncertain_delete(store, Some(Fault::Sqlite(path)))).await;
}
#[compio::test]
async fn postgres_collect_confirmation_failure_leaves_recoverable_delete() {
    let database = PostgresFixture::start().await;
    let client = connect(&database.admin_url).await;
    Box::pin(uncertain_delete(
        Rc::new(database.store.clone()),
        Some(Fault::Postgres(Box::new(client))),
    ))
    .await;
}

pub(super) async fn lost_reply(store: Rc<OrmStore>) {
    Box::pin(uncertain_delete(store, None)).await;
}

async fn uncertain_delete(store: Rc<OrmStore>, fault: Option<Fault>) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    if let Some(fault) = &fault {
        fault.confirmation(true).await;
    } else {
        fixture
            .backend
            .faults
            .lock()
            .unwrap()
            .push(super::fixture::Fault::LostDeleteReply(id.clone()));
    }
    let grant = Grant::new(fixture.scope.app_id());
    let receipt = fixture.scope.collect_job(&grant, options(1)).await.unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    assert!(
        !fixture.exists(fixture.scope.app_id(), &id).await,
        "the external deletion happened before the injected failure"
    );
    let pending = fixture.payload(&id).await;
    assert_eq!(pending.state, "deleting");
    assert_eq!(pending.expires_at, 0);
    assert_eq!(fixture.page(&grant.delivery.job).await.next_index, 1);
    let closed = fixture.scan().await;
    let reopened = fixture.reopen(true).await;
    assert_eq!(
        reopened
            .collect_job(&grant.retry(), options(1))
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(fixture.backend.calls(), std::slice::from_ref(&id));
    assert_eq!(fixture.payload(&id).await, pending);
    assert_eq!(fixture.scan().await, closed);
    if let Some(fault) = &fault {
        fault.confirmation(false).await;
    }
    assert_eq!(
        reopened
            .collect_job(&Grant::new(fixture.scope.app_id()), options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    let recovered = fixture.payload(&id).await;
    assert_eq!(recovered.state, "deleted");
    assert!(recovered.expires_at > pending.expires_at);
    assert!(!fixture.exists(fixture.scope.app_id(), &id).await);
    assert_eq!(fixture.backend.calls(), [id.clone(), id]);
}

async fn rollback(store: Rc<OrmStore>, fault: Fault) {
    let fixture = Fixture::new(store).await;
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    let grant = Grant::new(fixture.scope.app_id());
    fault.set(true).await;
    assert!(fixture.scope.collect_job(&grant, options(1)).await.is_err());
    assert_eq!(fixture.payload(&id).await.state, "deleted");
    assert_eq!(fixture.backend.calls(), std::slice::from_ref(&id));
    assert_eq!(fixture.page(&grant.delivery.job).await.next_index, 1);
    let uncommitted = fixture.scan().await;
    assert_eq!(uncommitted.revision, 1);
    assert!(uncommitted.after_id.is_none());
    assert_eq!(uncommitted.upper_id.as_deref(), Some(id.as_str()));
    assert!(fixture
        .scope
        .job_receipt(&grant.delivery.job)
        .await
        .unwrap()
        .is_none());
    fault.set(false).await;
    let reopened = fixture.reopen(true).await;
    assert_eq!(
        reopened
            .collect_job(&grant.retry(), options(1))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(fixture.backend.calls(), [id]);
    let committed = fixture.scan().await;
    assert_eq!(committed.revision, uncommitted.revision + 1);
    assert!(
        committed.after_id.is_none()
            && committed.upper_id.is_none()
            && committed.observed_at.is_none()
    );
}
