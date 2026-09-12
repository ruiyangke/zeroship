use super::*;

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
    use zeroship_data_sql::compile::validate_collection;
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
