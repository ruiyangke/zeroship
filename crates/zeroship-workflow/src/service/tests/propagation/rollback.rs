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
                let sql = if enabled {
                    format!("CREATE TRIGGER fail_propagation BEFORE INSERT ON __zeroship_workflow_{table} BEGIN SELECT RAISE(ABORT, 'injected propagation failure'); END")
                } else {
                    "DROP TRIGGER fail_propagation".into()
                };
                compio::runtime::spawn_blocking(move || {
                    rusqlite::Connection::open(path)
                        .unwrap()
                        .execute_batch(&sql)
                        .unwrap();
                })
                .await
                .unwrap();
            }
            Self::Postgres(client) => client
                .batch_execute(&if enabled {
                    format!("CREATE FUNCTION customer.fail_propagation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected propagation failure'; END $$; CREATE TRIGGER fail_propagation BEFORE INSERT ON customer.__zeroship_workflow_{table} FOR EACH ROW EXECUTE FUNCTION customer.fail_propagation();")
                } else {
                    format!("DROP TRIGGER fail_propagation ON customer.__zeroship_workflow_{table}; DROP FUNCTION customer.fail_propagation();")
                })
                .await
                .unwrap(),
        }
    }
}

#[compio::test]
async fn sqlite_failed_propagation_page_rolls_back_cursor_effects_and_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    let store = Rc::new(sqlite_store(&path).await);
    Box::pin(rollback(store, Database::Sqlite(path))).await;
}

#[compio::test]
async fn postgres_failed_propagation_page_rolls_back_cursor_effects_and_receipt() {
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
    let scope = service.fixture_app(app.clone());
    let parent = seed_runs(&service, &app, "Example", 1).await.remove(0);
    let children = seed_runs(&service, &app, "Child", 2).await;
    update_runs(
        &service,
        &app,
        &children,
        json!({"parent_id":parent, "parent_generation":0, "cascade":1, "depth":1}),
    )
    .await;
    cancel_idle(&scope, &parent).await;
    let page = open_page(&scope).await;
    let grant = JobGrant::new(&page);
    let options = PropagationOptions { page_size: 1 };
    let before = snapshot(&service, &app).await;
    // Fail at the child's Advance publication, the successor page publication
    // and the page record itself.
    for table in ["job_publications", "propagation_pages"] {
        database.fault(table, true).await;
        assert!(scope.propagation_job(&grant, options).await.is_err());
        assert_eq!(snapshot(&service, &app).await, before);
        assert!(scope.job_receipt(&page).await.unwrap().is_none());
        database.fault(table, false).await;
    }
    let obligation = rows(&service, "propagations", json!({"app_id":app.as_str()}))
        .await
        .remove(0);
    assert_eq!(obligation.optional_text("cursor").unwrap(), None);
    assert_eq!(obligation.integer("revision").unwrap(), 1);
    let receipt = scope
        .propagation_job(&grant.retry(), options)
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});
    let mut changed = 0;
    for child in &children {
        if run_row(&service, &app, child)
            .await
            .text("control")
            .unwrap()
            == "cancel"
        {
            changed += 1;
        }
    }
    assert_eq!(changed, 1);
    assert_exact_replay(&service, &scope, &page, &receipt).await;
    let rest = deliver_propagations_with(&scope, options).await;
    assert_eq!(rest.last().unwrap().outcome, JobOutcome::Completed {});
    for child in &children {
        assert_eq!(
            run_row(&service, &app, child)
                .await
                .text("control")
                .unwrap(),
            "cancel"
        );
    }
}
