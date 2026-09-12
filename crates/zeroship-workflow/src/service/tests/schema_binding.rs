use super::*;

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
        vec![(
            "orders".into(),
            zeroship_data_orm::value!({
                "id": {"type":"string", "primaryKey":true, "writable":true, "required":true},
                "body": {"type":"string", "required":true}
            }),
        )],
    )
    .unwrap();
    database
        .install_mask_policy(zeroship_data_orm::value!({}))
        .unwrap();
    assert!(database.collection("__zeroship_workflow_runs").is_err());
    let orders = database.collection("orders").unwrap();
    orders
        .insert(zeroship_data_orm::value!({"id":"native", "body":"native ORM data"}))
        .await
        .unwrap();
    let (service, app, _) = registered_service(store).await;
    let started = service
        .for_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let before = service
        .for_app(app.clone())
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
            .for_app(app)
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

#[test]
fn schema_binding_preserves_literals_and_includes_compiler_generated_names() {
    let output = Command::new("node")
        .args(["--input-type=module", "--eval"])
        .arg(r#"
import assert from 'node:assert/strict';
import { bindOwnedNames } from './schema/names.mjs';
const identifiers = new Set(['journal']);
const columns = new Set(['id', 'body']);
const sql = `CREATE TABLE "journal" (id TEXT, body TEXT DEFAULT 'journal '' "journal" CREATE INDEX "literal"', CONSTRAINT "journal_pkey" PRIMARY KEY (id, body), FOREIGN KEY (id) REFERENCES journal(id));
CREATE INDEX IF NOT EXISTS "journal_id_idx" ON "journal" (id);`;
const bound = bindOwnedNames(sql, { identifiers, columns });
assert.ok(bound.includes(`DEFAULT 'journal '' "journal" CREATE INDEX "literal"'`));
assert.ok(bound.includes('REFERENCES "__zeroship_workflow_journal"(id)'));
assert.ok(bound.includes('CONSTRAINT "__zeroship_workflow_journal_pkey"'));
assert.ok(bound.includes('INDEX IF NOT EXISTS "__zeroship_workflow_journal_id_idx"'));
assert.ok(bound.includes('ON "__zeroship_workflow_journal" (id)'));
for (const name of ['journal', 'journal_pkey', 'journal_id_idx']) {
    assert.throws(() => bindOwnedNames(sql, { identifiers, columns: new Set([name]) }), /collides with a column/);
}
assert.throws(() => bindOwnedNames(sql, { identifiers: new Set(['omitted']), columns }), /omitted owned identifier/);
assert.throws(() => bindOwnedNames('', { identifiers: new Set(), columns }), /no owned identifiers/);
assert.throws(() => bindOwnedNames('', { identifiers: new Set(['a'.repeat(64)]), columns }), /PostgreSQL limit/);
assert.throws(() => bindOwnedNames('', { identifiers: new Set(['untrusted"']), columns }), /invalid workflow owned name/);
"#)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
fn creator_queries_cannot_name_journal_tables() {
    use zeroship_data_orm::sql::compile::validate_collection;
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
            validate_collection(&name).is_err(),
            "creator query accepted {name}"
        );
    }
}
