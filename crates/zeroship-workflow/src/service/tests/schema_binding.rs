use super::*;

zeroship_data_orm::orm::schema! {
    business {
        orders {
            #[orm(primary_key)]
            id: Text,
            body: Text,
        }
    }
}

#[compio::test]
async fn workflow_tables_share_the_app_database_without_changing_business_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TABLE orders (id TEXT PRIMARY KEY, body TEXT NOT NULL); INSERT INTO orders VALUES ('order', 'customer data'); CREATE VIEW visible_orders AS SELECT * FROM orders;").unwrap();
    schema::initialize_sqlite(&path).unwrap();
    let store = Rc::new(sqlite_store(&path).await);
    let database = zeroship_data_orm::orm::Database::from_schema(
        store.binding.clone(),
        store.backend.clone(),
        business::schema(),
    )
    .unwrap();
    database
        .install_mask_policy(zeroship_data_orm::value!({}))
        .unwrap();
    let orders = database.collection("orders").unwrap();
    orders
        .insert(zeroship_data_orm::value!({"id":"native", "body":"native ORM data"}))
        .await
        .unwrap();
    let store = Rc::new(
        OrmStore::new(
            database.context().clone(),
            store.binding.clone(),
            store.backend.clone(),
            store.clock.clone(),
        )
        .unwrap(),
    );
    let (service, app, _, _deployments) = registered_service(store).await;
    let started = service
        .fixture_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let before = service
        .fixture_app(app.clone())
        .status(&started.id)
        .await
        .unwrap()
        .state;
    schema::initialize_sqlite(&path).unwrap();
    let zeroship_data_orm::orm::Output::Rows { rows, .. } = orders
        .find(
            zeroship_data_orm::value!({"id":"native"}),
            zeroship_data_orm::value!({}),
        )
        .await
        .unwrap()
    else {
        panic!("expected business rows");
    };
    assert_eq!(
        rows[0]["body"],
        zeroship_data_orm::value!("native ORM data")
    );
    assert_eq!(
        service
            .fixture_app(app)
            .status(&started.id)
            .await
            .unwrap()
            .state,
        before
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT body FROM visible_orders WHERE id='order'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "customer data"
    );
    connection
        .execute(
            "UPDATE __zeroship_workflow_schema_version SET fingerprint='incompatible'",
            [],
        )
        .unwrap();
    assert!(schema::initialize_sqlite(&path).is_err());
    assert_eq!(
        connection
            .query_row("SELECT body FROM orders WHERE id='order'", [], |row| row
                .get::<_, String>(
                0
            ))
            .unwrap(),
        "customer data"
    );
}

#[compio::test]
async fn conflicting_journal_metadata_cannot_replace_the_app_descriptor() {
    use zeroship_data_orm::{
        descriptor,
        orm::Database,
        schema::{CollectionSchema, ColumnSchema, LogicalType, Schema},
    };
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    let table = "__zeroship_workflow_schema_version";
    let mut id = ColumnSchema::new(LogicalType::Text);
    id.primary_key = true;
    let mut fingerprint = ColumnSchema::new(LogicalType::Text);
    fingerprint.required = false;
    let fields = CollectionSchema::new([("id".into(), id), ("fingerprint".into(), fingerprint)]);
    let database = Database::from_schema(
        store.binding.clone(),
        store.backend.clone(),
        Schema::new([(table.into(), fields.clone())]),
    )
    .unwrap();
    assert!(OrmStore::new(
        database.context().clone(),
        store.binding.clone(),
        store.backend.clone(),
        store.clock.clone(),
    )
    .is_err());
    let retained = database
        .context()
        .with(|| descriptor::collection_schema(&store.binding, table).unwrap());
    assert_eq!(retained.as_ref(), fields.fields());
    assert!(database.collection("__zeroship_workflow_requests").is_err());
}

#[test]
fn partial_workflow_schema_is_refused_in_a_shared_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TABLE orders (id TEXT); CREATE TABLE __zeroship_workflow_partial (value TEXT); INSERT INTO __zeroship_workflow_partial VALUES ('preserve');").unwrap();
    assert!(schema::initialize_sqlite(&path).is_err());
    assert_eq!(
        connection
            .query_row("SELECT value FROM __zeroship_workflow_partial", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "preserve"
    );
}

/// The engine binds the journal's schema through the ONE substitution in
/// `zeroship-workflow-schema`, which cannot depend on the ORM. This is what
/// holds that leaf's quoting rule to the ORM's: they must produce the same
/// bytes, or a schema name carrying a quote would bind differently in the
/// installer and in the host that reads it.
#[test]
fn the_leaf_substitution_agrees_with_the_orm_quoting_rule() {
    for name in ["customer", "a\"b", "a\"; DROP SCHEMA public; --"] {
        let bound = zeroship_workflow_schema::postgres_sql(name);
        let through_orm = zeroship_workflow_schema::POSTGRES_TEMPLATE.replace(
            zeroship_workflow_schema::SCHEMA_PLACEHOLDER,
            &zeroship_data_orm::sql::mapping::quote_ident(name),
        );
        assert_eq!(bound, through_orm, "substitution diverged for {name:?}");
    }
}

#[test]
fn sqlite_journal_objects_and_foreign_keys_stay_in_the_reserved_namespace() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(schema::SQLITE_SQL).unwrap();
    let names = conn
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','index')")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!names.is_empty());
    for name in names {
        assert!(
            name.starts_with("__zeroship_workflow_")
                || name.starts_with("sqlite_autoindex___zeroship_workflow_"),
            "unreserved journal object {name}"
        );
    }
    let references = conn.prepare("SELECT fk.\"table\" FROM sqlite_master m JOIN pragma_foreign_key_list(m.name) fk WHERE m.type='table'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0)).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert!(!references.is_empty());
    for name in references {
        assert!(
            name.starts_with("__zeroship_workflow_"),
            "unreserved foreign key target {name}"
        );
    }
}

#[test]
fn orm_queries_can_name_all_journal_tables() {
    use zeroship_data_orm::sql::mapping::validate_collection;
    assert!(validate_collection("orders").is_ok());
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(schema::SQLITE_SQL).unwrap();
    let names = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!names.is_empty());
    for name in names {
        assert!(
            validate_collection(&name).is_ok(),
            "ORM query refused journal table {name}"
        );
    }
}
