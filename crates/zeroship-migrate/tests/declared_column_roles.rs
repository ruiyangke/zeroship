//! Declared keys and reference targets reach real databases without name conventions.

#[path = "../../../tests/fixtures/postgres/mod.rs"]
mod postgres_fixture;

use serde_json::json;
use zeroship_migrate::schema::query::{FkEmission, build_create_table_with_fks_for_dialect};
use zeroship_migrate::{EffectivePolicy, effective_policy_from_charter_toml};
use zeroship_migrate_ir::dialect::DialectId;

fn policy() -> EffectivePolicy {
    effective_policy_from_charter_toml(r#"policy_version = 1
[[inject]]
scope = "all"
mandatory = true
primary_key = ["record_key"]
author_primary_key = "forbid"
columns = [
  { name = "record_key", type = "text", nullable = false, assign = { by = "typedId", on = "insert" } },
]
"#).expect("declared-key policy")
}

fn ddl(schema: &str, dialect: &DialectId) -> String {
    let parent = json!({
        "record_key": {"type": "id", "idPrefix": "item"},
        "id": {"type": "string"},
        "created_at": {"type": "string"},
    });
    let child = json!({
        "parent_key": {"type": "string", "refTarget": "parents", "refColumn": "record_key"},
    });
    [("parents", parent), ("children", child)]
        .into_iter()
        .map(|(table, fields)| {
            build_create_table_with_fks_for_dialect(
                zeroship_migrate::shipping_vendors(),
                schema,
                table,
                &fields,
                &FkEmission::Inline,
                dialect,
                &policy(),
            )
            .expect("declared fields render")
        })
        .collect::<Vec<_>>()
        .join(";\n")
}

#[test]
fn postgres_enforces_the_declared_reference_target() {
    let server = postgres_fixture::Postgres::start();
    let mut db =
        postgres::Client::connect(&server.url(), postgres::NoTls).expect("connect PostgreSQL");
    db.batch_execute(&ddl("public", &zeroship_migrate_postgres::DIALECT))
        .expect("create declared schema");
    db.batch_execute("INSERT INTO parents (record_key, id, created_at) VALUES ('parent', 'ordinary', 'free text');
        INSERT INTO children (record_key, parent_key) VALUES ('child', 'parent');
        UPDATE parents SET id = 'changed', created_at = 'still text' WHERE record_key = 'parent'")
        .expect("ordinary names remain writable");
    let value: String = db
        .query_one("SELECT id FROM parents WHERE record_key = 'parent'", &[])
        .expect("read declared key")
        .get(0);
    assert_eq!(value, "changed");
    let error = db
        .execute(
            "INSERT INTO children (record_key, parent_key) VALUES ('orphan', 'missing')",
            &[],
        )
        .expect_err("foreign key must be enforced");
    assert_eq!(
        error.code(),
        Some(&postgres::error::SqlState::FOREIGN_KEY_VIOLATION)
    );
}

#[test]
fn sqlite_enforces_the_declared_reference_target() {
    let dir = tempfile::tempdir().expect("SQLite directory");
    let db = rusqlite::Connection::open(dir.path().join("app.sqlite")).expect("open SQLite file");
    db.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    db.execute_batch(&ddl("main", &zeroship_migrate_sqlite::DIALECT))
        .expect("create declared schema");
    db.execute_batch("INSERT INTO parents (record_key, id, created_at) VALUES ('parent', 'ordinary', 'free text');
        INSERT INTO children (record_key, parent_key) VALUES ('child', 'parent');
        UPDATE parents SET id = 'changed', created_at = 'still text' WHERE record_key = 'parent'")
        .expect("ordinary names remain writable");
    let value: String = db
        .query_row(
            "SELECT id FROM parents WHERE record_key = 'parent'",
            [],
            |row| row.get(0),
        )
        .expect("read declared key");
    assert_eq!(value, "changed");
    let error = db
        .execute(
            "INSERT INTO children (record_key, parent_key) VALUES ('orphan', 'missing')",
            [],
        )
        .expect_err("foreign key must be enforced");
    assert!(matches!(error, rusqlite::Error::SqliteFailure(code, _)
        if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY));
}
