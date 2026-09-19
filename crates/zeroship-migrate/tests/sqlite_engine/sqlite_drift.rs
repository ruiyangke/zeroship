//! Drift proofs for the `SQLite` migration backend.
//! Real temp-file `SQLite` throughout — `snapshot_schema` over the live app
//! file, checksum drift over the journal, sentinel recovery from `sqlite_master`.

use crate::support;

use std::path::PathBuf;

use serde_json::{json, Value};
use tempfile::TempDir;
use zeroship_migrate::apply::backend::MigrationBackend;
use zeroship_migrate::apply::drift::{diff_snapshots, StructuralDrift};
use zeroship_migrate::conn::ExecutorConfig;
use zeroship_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zeroship_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zeroship_migrate::model::snapshot::IdDefaultSnapshot;
use zeroship_migrate::{fold_ops, fold_ops_onto, IrAuthor, LiveSchema, SchemaSnapshot};
use zeroship_migrate_sqlite::SqliteBackend;

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(app_id: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{app_id}.sqlite"));
    let journal = dir.path().join(format!("zs-{app_id}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn mig(name: &str, up: &str) -> Migration {
    let flags = MigrationFlags::default();
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down: None,
        flags: &flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version: MigrationId::generate(),
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum,
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
        effect: None,
    }
}

fn cfg() -> ExecutorConfig {
    ExecutorConfig::new("prj_test", "app", support::no_inject("app"))
}

fn lower_id_table_sql(
    table: &str,
    ty: &str,
    id_prefix: Option<&str>,
    uuid_v4_default: bool,
) -> String {
    let mut column = json!({
        "name": "id",
        "type": ty,
        "nullable": false,
    });
    let object = column.as_object_mut().expect("column fixture is an object");
    if let Some(id_prefix) = id_prefix {
        object.insert("type".to_string(), json!({ "string": { "length": 36 } }));
        object.insert("idPrefix".to_string(), json!(id_prefix));
    }
    if uuid_v4_default {
        object.insert(
            "default".to_string(),
            json!({ "expr": { "node": "uuidV4" } }),
        );
    }
    let ir: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": format!("create_{table}"),
        "owner_app": "app_sqlite_drift",
        "ops": [{
            "op": "createTable",
            "name": table,
            "columns": [column],
            "primaryKey": ["id"],
            "indexes": [],
        }],
    }))
    .expect("ID table IR must deserialize");
    let migrations = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        "main",
        "app_sqlite_drift",
        &zeroship_migrate_sqlite::DIALECT,
        &support::no_inject("main"),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("ID table must lower for SQLite");
    let marker = format!("CREATE TABLE \"{table}\"");
    migrations
        .into_iter()
        .find(|migration| migration.up.starts_with(&marker))
        .unwrap_or_else(|| panic!("missing {marker} migration"))
        .up
}

fn replace_table(p: &Paths, table: &str, create_sql: &str) {
    let conn = rusqlite::Connection::open(&p.app).expect("reopen app file raw");
    conn.execute_batch("PRAGMA foreign_keys = OFF;")
        .expect("disable FK enforcement for out-of-band replacement");
    conn.execute_batch(&format!("DROP TABLE \"{table}\";"))
        .expect("drop table out of band");
    conn.execute_batch(create_sql)
        .expect("recreate table out of band");
}

fn assert_column_drift(
    drift: &StructuralDrift,
    column: &str,
    field: &str,
    expected: &str,
    actual: &str,
) {
    assert!(
        drift.altered_objects.iter().any(|altered| {
            altered.object == format!("column {column}")
                && altered.field == field
                && altered.expected == expected
                && altered.actual == actual
        }),
        "expected {column}.{field} drift {expected:?} -> {actual:?}: {drift:?}"
    );
}

fn assert_constraint_definition_drift(
    drift: &StructuralDrift,
    constraint: &str,
    expected_fragment: &str,
    actual_fragment: &str,
) {
    assert!(
        drift.altered_objects.iter().any(|altered| {
            altered.object == format!("constraint {constraint}")
                && altered.field == "definition"
                && altered.expected.contains(expected_fragment)
                && altered.actual.contains(actual_fragment)
        }),
        "expected {constraint} definition drift containing {expected_fragment:?} -> \
         {actual_fragment:?}: {drift:?}"
    );
}

/// SQLite exposes a rowid primary key under a synthetic `pk_<table>` constraint
/// and no separate index, while the authored fold uses `<table>_pkey` for both.
/// That catalog-name artifact predates drift introspection and is unrelated to
/// the column facets under test; remove only those PK-owned objects before the
/// authored-vs-live clean comparison.
fn strip_sqlite_primary_key_catalog_noise(snapshot: &mut SchemaSnapshot) {
    for table in snapshot.tables.values_mut() {
        let primary_key_names = table
            .constraints
            .iter()
            .filter(|constraint| constraint.kind == "PRIMARY KEY")
            .map(|constraint| constraint.name.clone())
            .collect::<Vec<_>>();
        table
            .constraints
            .retain(|constraint| constraint.kind != "PRIMARY KEY");
        table
            .indexes
            .retain(|index| !primary_key_names.contains(&index.name));
    }
}

// ---------------------------------------------------------------------------
// snapshot_schema reflects an applied CREATE TABLE: the table + its columns +
// PK constraint show up in the dialect-agnostic SchemaSnapshot.
// ---------------------------------------------------------------------------
#[compio::test]
async fn snapshot_reflects_applied_table() {
    let p = paths("snap1");
    let be = backend(&p);
    be.apply_one_additive(
        &mig(
            "create_users",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL, age INTEGER);",
        ),
        "deployer",
    )
    .await
    .expect("apply");

    let snap = be.snapshot_schema_sqlite().await.expect("snapshot");
    let users = snap.tables.get("users").expect("users table in snapshot");
    let col_names: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(col_names, vec!["age", "email", "id"], "columns name-sorted");

    // email is NOT NULL; age is nullable.
    let email = users.columns.iter().find(|c| c.name == "email").unwrap();
    assert!(!email.nullable, "email NOT NULL");
    let age = users.columns.iter().find(|c| c.name == "age").unwrap();
    assert!(age.nullable, "age nullable");

    // The PRIMARY KEY surfaces as a constraint (matching the PG snapshot's
    // constraint bucket).
    assert!(
        users.constraints.iter().any(|c| c.kind == "PRIMARY KEY"),
        "PK constraint present: {:?}",
        users.constraints
    );

    // Internal/journal objects excluded: no `sqlite_*` or `_mig` table leaks in.
    assert!(
        snap.tables.keys().all(|k| !k.starts_with("sqlite_")),
        "no sqlite internal tables"
    );
    assert!(
        !snap.tables.contains_key("schema_migrations"),
        "journal objects must not appear in the app-schema snapshot"
    );
}

// ---------------------------------------------------------------------------
// A clean schema (snapshot vs itself) shows ZERO structural drift.
// ---------------------------------------------------------------------------
#[compio::test]
async fn clean_schema_zero_structural_drift() {
    let p = paths("snap_clean");
    let be = backend(&p);
    be.apply_one_additive(
        &mig("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);"),
        "d",
    )
    .await
    .expect("apply");

    let snap = be.snapshot_schema_sqlite().await.expect("snapshot");
    // The snapshot diffed against itself is clean.
    let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &snap, &snap);
    assert!(drift.is_clean(), "self-diff must be clean: {drift:?}");
}

// ---------------------------------------------------------------------------
// SQLite has two distinct generated-integer contracts: the ordinary rowid
// allocator and INTEGER PRIMARY KEY AUTOINCREMENT. Both must round-trip cleanly,
// while an out-of-band add/drop of AUTOINCREMENT is identity drift.
// ---------------------------------------------------------------------------
#[compio::test]
async fn rowid_and_autoincrement_identity_are_introspected_and_drift_compared() {
    const BARE: &str = "CREATE TABLE items (id INTEGER PRIMARY KEY, payload TEXT NOT NULL);";
    const AUTO: &str =
        "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT NOT NULL);";

    for (tag, sql, expected_identity) in [
        ("identity_clean_rowid", BARE, None),
        ("identity_clean_auto", AUTO, Some(false)),
    ] {
        let p = paths(tag);
        let be = backend(&p);
        be.apply_one_additive(&mig(tag, sql), "d")
            .await
            .expect("apply identity fixture");
        let expected = be.snapshot_schema_sqlite().await.expect("first snapshot");
        let actual = be.snapshot_schema_sqlite().await.expect("second snapshot");
        assert!(
            diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual).is_clean(),
            "an unchanged identity fixture must stay clean"
        );
        let id = expected.tables["items"]
            .columns
            .iter()
            .find(|column| column.name == "id")
            .expect("id column");
        assert!(id.rowid_alias, "exact INTEGER PRIMARY KEY is a rowid alias");
        assert_eq!(
            id.identity.map(|identity| identity.always),
            expected_identity,
            "only explicit AUTOINCREMENT carries the identity facet"
        );
    }

    for (tag, expected_sql, changed_sql, expected, actual) in [
        (
            "identity_drop",
            AUTO,
            BARE,
            "rowid alias, auto increment",
            "rowid alias",
        ),
        (
            "identity_add",
            BARE,
            AUTO,
            "rowid alias",
            "rowid alias, auto increment",
        ),
    ] {
        let p = paths(tag);
        let be = backend(&p);
        be.apply_one_additive(&mig(tag, expected_sql), "d")
            .await
            .expect("apply expected identity shape");
        let expected_snapshot = be
            .snapshot_schema_sqlite()
            .await
            .expect("expected snapshot");
        drop(be);
        replace_table(&p, "items", changed_sql);
        let actual_snapshot = backend(&p)
            .snapshot_schema_sqlite()
            .await
            .expect("actual snapshot");
        let drift = diff_snapshots(
            zeroship_migrate::shipping_vendors(),
            &expected_snapshot,
            &actual_snapshot,
        );
        assert_column_drift(&drift, "id", "identity", expected, actual);
    }

    const ROWID_DEFAULT: &str =
        "CREATE TABLE items (id INTEGER PRIMARY KEY DEFAULT 7, payload TEXT NOT NULL);";
    for (tag, expected_sql, changed_sql, expected, actual) in [
        ("rowid_default_add", BARE, ROWID_DEFAULT, "absent", "7"),
        ("rowid_default_drop", ROWID_DEFAULT, BARE, "7", "absent"),
    ] {
        let p = paths(tag);
        let be = backend(&p);
        be.apply_one_additive(&mig(tag, expected_sql), "d")
            .await
            .expect("apply expected rowid default shape");
        let expected_snapshot = be
            .snapshot_schema_sqlite()
            .await
            .expect("expected rowid snapshot");
        drop(be);
        replace_table(&p, "items", changed_sql);
        let actual_snapshot = backend(&p)
            .snapshot_schema_sqlite()
            .await
            .expect("actual rowid snapshot");
        let drift = diff_snapshots(
            zeroship_migrate::shipping_vendors(),
            &expected_snapshot,
            &actual_snapshot,
        );
        assert_column_drift(&drift, "id", "default", expected, actual);
    }
}

// ---------------------------------------------------------------------------
// UUID defaults are compared semantically on the ID-bearing surface. Ordinary
// emission-only defaults remain deliberately ignored.
// ---------------------------------------------------------------------------
#[compio::test]
async fn uuid_id_defaults_detect_add_remove_and_swap_without_cosmetic_drift() {
    let with_default = lower_id_table_sql("ids", "uuid", None, true);
    let without_default = lower_id_table_sql("ids", "uuid", None, false);
    let swapped_default =
        without_default.replacen(" CHECK", " DEFAULT (lower(hex(randomblob(16)))) CHECK", 1);
    assert_ne!(
        swapped_default, without_default,
        "UUID fixture must contain its format CHECK"
    );

    for (tag, expected_sql, changed_sql, expected, actual) in [
        (
            "uuid_default_remove",
            with_default.as_str(),
            without_default.as_str(),
            "uuidV4",
            "absent",
        ),
        (
            "uuid_default_add",
            without_default.as_str(),
            with_default.as_str(),
            "absent",
            "uuidV4",
        ),
        (
            "uuid_default_swap",
            with_default.as_str(),
            swapped_default.as_str(),
            "uuidV4",
            "call:lower(call:hex(call:randomblob(literal:16)))",
        ),
    ] {
        let p = paths(tag);
        let be = backend(&p);
        be.apply_one_additive(&mig(tag, expected_sql), "d")
            .await
            .expect("apply expected UUID default");
        let expected_snapshot = be
            .snapshot_schema_sqlite()
            .await
            .expect("expected snapshot");
        let id = expected_snapshot.tables["ids"]
            .columns
            .iter()
            .find(|column| column.name == "id")
            .expect("UUID id column");
        assert_eq!(
            id.id_default,
            Some(if expected == "uuidV4" {
                IdDefaultSnapshot::UuidV4
            } else {
                IdDefaultSnapshot::Absent
            })
        );
        let clean = be.snapshot_schema_sqlite().await.expect("clean snapshot");
        assert!(
            diff_snapshots(zeroship_migrate::shipping_vendors(), &expected_snapshot, &clean).is_clean(),
            "unchanged UUID defaults must stay clean"
        );
        drop(be);

        replace_table(&p, "ids", changed_sql);
        let actual_snapshot = backend(&p)
            .snapshot_schema_sqlite()
            .await
            .expect("actual snapshot");
        let drift = diff_snapshots(
            zeroship_migrate::shipping_vendors(),
            &expected_snapshot,
            &actual_snapshot,
        );
        assert_column_drift(&drift, "id", "default", expected, actual);
    }

    let lowercase_literal = without_default.replacen(
        " CHECK",
        " DEFAULT 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa' CHECK",
        1,
    );
    let uppercase_literal = without_default.replacen(
        " CHECK",
        " DEFAULT 'AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA' CHECK",
        1,
    );
    let p = paths("uuid_literal_case_drift");
    let be = backend(&p);
    be.apply_one_additive(&mig("uuid_literal_case_drift", &lowercase_literal), "d")
        .await
        .expect("apply lowercase UUID literal default");
    let expected = be
        .snapshot_schema_sqlite()
        .await
        .expect("expected snapshot");
    drop(be);
    replace_table(&p, "ids", &uppercase_literal);
    let actual = backend(&p)
        .snapshot_schema_sqlite()
        .await
        .expect("uppercase UUID literal snapshot");
    assert_column_drift(
        &diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual),
        "id",
        "default",
        "\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\"",
        "\"AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA\"",
    );

    // An ordinary (non-ID) default on `label`. Two questions must stay distinct:
    // redundant grouping is cosmetic and must stay clean, while a changed value is
    // a changed default and must be reported (`comparable_column_default`).
    let p = paths("ordinary_default_cosmetic");
    let be = backend(&p);
    be.apply_one_additive(
        &mig(
            "ordinary_default",
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, label TEXT DEFAULT 'draft');",
        ),
        "d",
    )
    .await
    .expect("apply ordinary default fixture");
    let expected = be
        .snapshot_schema_sqlite()
        .await
        .expect("expected snapshot");
    drop(be);
    replace_table(
        &p,
        "notes",
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, label TEXT DEFAULT ('draft'));",
    );
    let actual = backend(&p)
        .snapshot_schema_sqlite()
        .await
        .expect("cosmetic default snapshot");
    assert!(
        diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual).is_clean(),
        "ordinary default spelling alone must not create drift"
    );

    replace_table(
        &p,
        "notes",
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, label TEXT DEFAULT ('published'));",
    );
    let actual = backend(&p)
        .snapshot_schema_sqlite()
        .await
        .expect("changed default snapshot");
    assert_column_drift(
        &diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual),
        "label",
        "default",
        "\"draft\"",
        "\"published\"",
    );
}

// ---------------------------------------------------------------------------
// Portable clean oracle: compare the authored SQLite fold—not another live
// snapshot—to the catalog. This guards the exact boundary most likely to
// phantom-drift: bigint identity folds to physical INTEGER rowid storage, while
// UUID/default and TypeID/ULID contracts must recover semantically.
// ---------------------------------------------------------------------------
#[compio::test]
async fn authored_identity_default_and_format_snapshot_matches_live_sqlite() {
    let ir: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "portable_id_facets",
        "owner_app": "app_sqlite_drift",
        "ops": [
            {
                "op": "createTable",
                "name": "portable_ids",
                "columns": [
                    {
                        "name": "auto_id",
                        "type": "bigInt",
                        "nullable": false,
                        "identity": { "always": false }
                    },
                    {
                        "name": "generated_uuid",
                        "type": "uuid",
                        "nullable": false,
                        "default": { "expr": { "node": "uuidV4" } }
                    },
                    { "name": "supplied_uuid", "type": "uuid", "nullable": false },
                    {
                        "name": "type_id",
                        "type": { "string": { "length": 36 } },
                        "nullable": false,
                        "idPrefix": "acct"
                    },
                    { "name": "ordinary", "type": "text", "nullable": true }
                ],
                "primaryKey": ["auto_id"],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "plain_bigint_ids",
                "columns": [{
                    "name": "id",
                    "type": "bigInt",
                    "nullable": false
                }],
                "primaryKey": ["id"],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "boolean_ids",
                "columns": [{
                    "name": "id",
                    "type": "boolean",
                    "nullable": false
                }],
                "primaryKey": ["id"],
                "indexes": []
            }
        ]
    }))
    .expect("portable SQLite ID fixture must deserialize");
    let mut expected = fold_ops(
        zeroship_migrate::shipping_vendors(),
        &ir.ops,
        &zeroship_migrate_sqlite::DIALECT,
        "main",
        &support::no_inject("app"),
    )
    .expect("portable SQLite ID fixture must fold");
    let migrations = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        "main",
        "app_sqlite_drift",
        &zeroship_migrate_sqlite::DIALECT,
        &support::no_inject("main"),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("portable SQLite ID fixture must lower");

    let p = paths("portable_id_facets_clean");
    let be = backend(&p);
    for migration in &migrations {
        be.apply_one_additive(migration, "d")
            .await
            .unwrap_or_else(|error| panic!("apply {}: {error}", migration.name));
    }
    let mut actual = be.snapshot_schema_sqlite().await.expect("live snapshot");
    strip_sqlite_primary_key_catalog_noise(&mut expected);
    strip_sqlite_primary_key_catalog_noise(&mut actual);

    let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual);
    assert!(
        drift.is_clean(),
        "authored portable ID facets must round-trip without phantom drift: {drift:#?}"
    );

    drop(be);
    replace_table(
        &p,
        "boolean_ids",
        "CREATE TABLE boolean_ids (id INTEGER PRIMARY KEY) WITHOUT ROWID;",
    );
    let mut changed = backend(&p)
        .snapshot_schema_sqlite()
        .await
        .expect("boolean non-rowid snapshot");
    strip_sqlite_primary_key_catalog_noise(&mut changed);
    assert_column_drift(
        &diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &changed),
        "id",
        "identity",
        "rowid alias",
        "",
    );
}

#[compio::test]
async fn catalog_seeded_fold_preserves_non_rowid_integer_primary_keys() {
    let p = paths("non_rowid_integer_primary_keys");
    let be = backend(&p);
    be.apply_one_additive(
        &mig(
            "non_rowid_integer_primary_keys",
            "CREATE TABLE without_rowid (id INTEGER PRIMARY KEY) WITHOUT ROWID; \
             CREATE TABLE descending_rowid (id INTEGER PRIMARY KEY DESC);",
        ),
        "d",
    )
    .await
    .expect("apply non-rowid SQLite fixture");

    let live = be.snapshot_schema_sqlite().await.expect("live snapshot");
    for table in ["without_rowid", "descending_rowid"] {
        assert!(
            !live.tables[table].columns[0].rowid_alias,
            "{table} must not be classified as a rowid alias"
        );
    }
    let projected = fold_ops_onto(
        zeroship_migrate::shipping_vendors(),
        &live,
        &[],
        &zeroship_migrate_sqlite::DIALECT,
        "main",
        &support::no_inject("app"),
    )
    .expect("empty catalog-seeded fold");
    let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &live, &projected);
    assert!(
        drift.is_clean(),
        "an empty fold must preserve WITHOUT ROWID and PRIMARY KEY DESC exclusions: {drift:#?}"
    );
}

#[compio::test]
async fn typed_reference_literal_defaults_use_expected_driven_catalog_comparison() {
    const LITERAL: &str = "account_00000000000000000000000000";
    const CHANGED_LITERAL: &str = "account_00000000000000000000000001";
    let ir: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "typed_reference_literal_default",
        "owner_app": "app_sqlite_drift",
        "ops": [
            {
                "op": "createTable",
                "name": "parents",
                "columns": [{
                    "name": "id",
                    "type":{"string":{"length":36}},"nullable": false,
                    "idPrefix":"acct",
                    "default": { "literal": { "value": LITERAL } }
                }],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "children",
                "columns": [{
                    "name": "parent_id",
                    "type":{"string":{"length":36}},"nullable": true,
                    "idPrefix":"acct",
                    "default": { "literal": { "value": LITERAL } },
                    "references": {
                        "table": "parents",
                        "column": "id",
                        "onDelete": "cascade",
                        "onUpdate": "cascade"
                    }
                }],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            }
        ]
    }))
    .expect("typed-reference literal fixture must deserialize");
    let mut expected = fold_ops(
        zeroship_migrate::shipping_vendors(),
        &ir.ops,
        &zeroship_migrate_sqlite::DIALECT,
        "main",
        &support::no_inject("app"),
    )
    .expect("typed-reference literal fixture must fold");
    let migrations = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        "main",
        "app_sqlite_drift",
        &zeroship_migrate_sqlite::DIALECT,
        &support::no_inject("main"),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("typed-reference literal fixture must lower");

    let p = paths("typed_reference_literal_default");
    let be = backend(&p);
    for migration in &migrations {
        be.apply_one_additive(migration, "d")
            .await
            .unwrap_or_else(|error| panic!("apply {}: {error}", migration.name));
    }
    let actual = be.snapshot_schema_sqlite().await.expect("live snapshot");
    let child = actual.tables["children"]
        .columns
        .iter()
        .find(|column| column.name == "parent_id")
        .expect("typed reference column");
    assert_eq!(
        child.id_default, None,
        "a typed reference intentionally has no local ID-format marker"
    );
    assert!(
        child
            .default
            .as_deref()
            .is_some_and(|default| default.contains(LITERAL)),
        "raw PRAGMA default must remain available for expected-driven comparison: {child:?}"
    );
    let parent = actual.tables["parents"]
        .columns
        .iter()
        .find(|column| column.name == "id")
        .expect("local TypeID column");
    assert_eq!(
        parent.id_default, None,
        "a typed-id column is a plain bounded string; its literal default is not an ID-format marker"
    );
    let child_create = actual.tables["children"]
        .stored_create_sql
        .clone()
        .expect("stored child CREATE");
    let mut clean = actual;
    strip_sqlite_primary_key_catalog_noise(&mut expected);
    strip_sqlite_primary_key_catalog_noise(&mut clean);
    let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &clean);
    assert!(
        drift.is_clean(),
        "authored typed-reference default must match its raw live catalog value: {drift:#?}"
    );
    drop(be);

    let rendered_literal = format!("DEFAULT '{LITERAL}'");
    let regrouped_literal = format!("DEFAULT (( '{LITERAL}' ))");
    let regrouped_create = child_create.replacen(&rendered_literal, &regrouped_literal, 1);
    assert_ne!(
        regrouped_create, child_create,
        "typed-reference default fixture must contain the rendered literal"
    );
    replace_table(&p, "children", &regrouped_create);
    let be = backend(&p);
    let mut regrouped = be
        .snapshot_schema_sqlite()
        .await
        .expect("regrouped literal snapshot");
    strip_sqlite_primary_key_catalog_noise(&mut regrouped);
    let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &regrouped);
    assert!(
        drift.is_clean(),
        "catalog-only default parentheses must normalize without drift: {drift:#?}"
    );
    drop(be);

    let changed_create = regrouped_create.replacen(LITERAL, CHANGED_LITERAL, 1);
    replace_table(&p, "children", &changed_create);
    let mut changed = backend(&p)
        .snapshot_schema_sqlite()
        .await
        .expect("changed literal snapshot");
    strip_sqlite_primary_key_catalog_noise(&mut changed);
    let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &changed);
    assert_column_drift(
        &drift,
        "parent_id",
        "default",
        &serde_json::to_string(LITERAL).expect("literal serializes"),
        &serde_json::to_string(CHANGED_LITERAL).expect("literal serializes"),
    );
}

// ---------------------------------------------------------------------------
// Out-of-band ALTER is DETECTED: snapshot the schema, then ALTER the app file
// directly (a new column the migrations never declared), re-snapshot, and the
// structural diff surfaces the unexpected column.
// ---------------------------------------------------------------------------
#[compio::test]
async fn out_of_band_alter_detected_as_structural_drift() {
    let p = paths("snap_drift");
    let be = backend(&p);
    be.apply_one_additive(
        &mig(
            "t",
            "CREATE TABLE widgets (id INTEGER PRIMARY KEY, label TEXT);",
        ),
        "d",
    )
    .await
    .expect("apply");

    // The expected snapshot (what the migrations declared).
    let expected = be
        .snapshot_schema_sqlite()
        .await
        .expect("expected snapshot");
    drop(be); // close the hardened connection so a raw conn can write the app file.

    // Out-of-band ALTER: add a column the migration journal knows nothing about,
    // via a PLAIN connection (simulating manual tampering of the app file).
    {
        let conn = rusqlite::Connection::open(&p.app).expect("reopen app file raw");
        conn.execute_batch("ALTER TABLE widgets ADD COLUMN sneaky TEXT;")
            .expect("out-of-band alter");
    }

    // Re-open the backend, re-snapshot the LIVE schema, and diff.
    let be2 = backend(&p);
    let actual = be2.snapshot_schema_sqlite().await.expect("actual snapshot");
    let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual);
    assert!(!drift.is_clean(), "drift must be detected");
    assert!(
        drift
            .unexpected_objects
            .iter()
            .any(|o| o.contains("sneaky")),
        "the out-of-band column must surface as unexpected: {drift:?}"
    );
}

// ---------------------------------------------------------------------------
// Checksum drift: a net-applied version whose journaled checksum no longer
// matches the supplied set's checksum is flagged (tamper / edited-after-apply).
// ---------------------------------------------------------------------------
#[compio::test]
async fn checksum_drift_detected_over_journal() {
    let p = paths("ck_drift");
    let be = backend(&p);
    let applied = mig("t", "CREATE TABLE t (id INTEGER PRIMARY KEY);");
    be.apply_one_additive(&applied, "d").await.expect("apply");

    // Clean: the SAME migration set ⇒ no drift.
    let clean = be
        .check_checksum_drift(&cfg(), std::slice::from_ref(&applied))
        .await
        .expect("drift read");
    assert!(clean.is_clean(), "matching set has no drift: {clean:?}");

    // Tamper: a migration with the SAME version but a DIFFERENT up ⇒ different
    // checksum ⇒ ChecksumDrift.
    let mut mutated = mig("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, extra TEXT);");
    mutated.version = applied.version.clone();
    let report = be
        .check_checksum_drift(&cfg(), std::slice::from_ref(&mutated))
        .await
        .expect("drift read");
    assert!(!report.is_clean(), "checksum drift must be detected");
    assert_eq!(report.checksum_drift.len(), 1);
    assert_eq!(report.checksum_drift[0].version, applied.version.as_str());
}

// ---------------------------------------------------------------------------
// Orphan journal: a version the database has APPLIED but which no longer exists
// in the supplied migration set - a migration file deleted after being applied.
// ---------------------------------------------------------------------------

/// The second bucket of `ChecksumDriftReport`, and until now the only one of the
/// drift buckets never proven to FILL. It appeared across the crate solely inside
/// `.is_empty()` assertions, which pass whether or not the detector can ever
/// report - the same gap closed for the structural buckets in the two preceding
/// commits.
///
/// The case is a real operational hazard rather than a hypothetical: someone
/// deletes a migration file from the bundle after it has been applied, and the
/// deployment then carries a database whose history nothing in the repository
/// explains.
#[compio::test]
async fn orphan_journal_detected_when_a_migration_is_deleted_from_the_set() {
    let p = paths("orphan_j");
    let be = backend(&p);
    let applied = mig("t", "CREATE TABLE t (id INTEGER PRIMARY KEY);");
    be.apply_one_additive(&applied, "d").await.expect("apply");

    // CONTROL FIRST: with the migration still present the report is clean, so a
    // later non-clean result is attributable to its removal and nothing else.
    let clean = be
        .check_checksum_drift(&cfg(), std::slice::from_ref(&applied))
        .await
        .expect("drift read");
    assert!(clean.is_clean(), "the intact set has no drift: {clean:?}");

    // DELETE IT FROM THE SET: applied in the database, absent from the bundle.
    let report = be
        .check_checksum_drift(&cfg(), &[])
        .await
        .expect("drift read");
    assert!(
        !report.is_clean(),
        "a deleted-but-applied migration must not read as clean: {report:?}"
    );
    assert!(
        report.checksum_drift.is_empty(),
        "nothing was EDITED, so the tamper bucket must stay empty - the two \
         buckets report different faults: {report:?}"
    );
    assert_eq!(
        report.orphan_journal.len(),
        1,
        "exactly the one applied version is orphaned: {report:?}"
    );
    assert_eq!(
        report.orphan_journal[0].version,
        applied.version.as_str(),
        "the report must name the orphaned version: {report:?}"
    );
}

// ---------------------------------------------------------------------------
// Sentinel recovery: an applied table whose CREATE text carries an inline
// `/* zero-migrate:mask:... */` sentinel (the emitter's SQLite-side wire) is recovered
// into the snapshot's comment_sentinel — not silently dropped.
// ---------------------------------------------------------------------------
#[compio::test]
async fn mask_sentinel_recovered_from_sqlite_master() {
    let p = paths("snap_sentinel");
    let be = backend(&p);
    // The emitter writes the inline mask sentinel onto the FIELD'S OWN column; the
    // real value lives in a `__zs_raw__<col>` sibling instead. sqlite_master.sql
    // preserves the comment verbatim. We hand-author the exact shape the emitter
    // produces today.
    let raw = zeroship_migrate::schema::query::raw_column_name("ssn");
    be.apply_one_additive(
        &mig(
            "with_mask",
            &format!(
                "CREATE TABLE accounts (\
                    id INTEGER PRIMARY KEY, \
                    {raw} BLOB, \
                    ssn TEXT /* zero-migrate:mask:kind=last4,classification=pii */);"
            ),
        ),
        "d",
    )
    .await
    .expect("apply masked table");

    let snap = be.snapshot_schema_sqlite().await.expect("snapshot");
    let accounts = snap.tables.get("accounts").expect("accounts table");
    let masked = accounts
        .columns
        .iter()
        .find(|c| c.name == "ssn")
        .expect("mask column present");
    assert_eq!(
        masked.comment_sentinel.as_deref(),
        Some("zero-migrate:mask:kind=last4,classification=pii"),
        "the inline mask sentinel must be recovered from sqlite_master.sql"
    );
    // A plain column carries no sentinel.
    let id = accounts.columns.iter().find(|c| c.name == "id").unwrap();
    assert_eq!(id.comment_sentinel, None);
}

// ---------------------------------------------------------------------------
// Composite foreign keys combine PRAGMA's authoritative ordered tuple/actions
// with the name + deferrability metadata retained in sqlite_master.sql.
// MATCH SIMPLE is canonicalized to the omitted default.
// ---------------------------------------------------------------------------
#[compio::test]
async fn composite_fk_introspection_round_trips_name_policy_and_detects_drift() {
    let p = paths("snap_composite_fk");
    let be = backend(&p);
    be.apply_one_additive(
        &mig(
            "parent",
            "CREATE TABLE parent (\
                tenant TEXT NOT NULL, \
                parent_id INTEGER NOT NULL, \
                PRIMARY KEY (tenant, parent_id));",
        ),
        "d",
    )
    .await
    .expect("apply parent");
    be.apply_one_additive(
        &mig(
            "child",
            "CREATE TABLE child (\
                tenant TEXT, \
                parent_id INTEGER, \
                CONSTRAINT fk_child_parent \
                    FOREIGN KEY (tenant, parent_id) \
                    REFERENCES parent (tenant, parent_id) \
                    MATCH SIMPLE \
                    ON DELETE CASCADE \
                    ON UPDATE SET NULL \
                    DEFERRABLE INITIALLY DEFERRED);",
        ),
        "d",
    )
    .await
    .expect("apply child");

    let expected = be.snapshot_schema_sqlite().await.expect("snapshot");
    let child = expected.tables.get("child").expect("child table");
    let foreign_key = child
        .constraints
        .iter()
        .find(|constraint| constraint.kind == "FOREIGN KEY")
        .expect("composite foreign key");
    assert_eq!(foreign_key.name, "fk_child_parent");
    assert_eq!(
        foreign_key.definition,
        "FOREIGN KEY (tenant, parent_id) REFERENCES parent(tenant, parent_id) \
         ON UPDATE SET NULL ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED"
    );
    assert!(
        !foreign_key.definition.contains("MATCH"),
        "MATCH SIMPLE must canonicalize to the portable omitted default"
    );

    drop(be);
    {
        let conn = rusqlite::Connection::open(&p.app).expect("reopen app file raw");
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF; \
             DROP TABLE child; \
             CREATE TABLE child (\
                 tenant TEXT, \
                 parent_id INTEGER, \
                 CONSTRAINT fk_child_parent \
                     FOREIGN KEY (tenant, parent_id) \
                     REFERENCES parent (tenant, parent_id) \
                     MATCH SIMPLE \
                     ON DELETE RESTRICT \
                     ON UPDATE CASCADE \
                     DEFERRABLE INITIALLY IMMEDIATE);",
        )
        .expect("replace child out of band");
    }

    let be = backend(&p);
    let actual = be.snapshot_schema_sqlite().await.expect("changed snapshot");
    let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual);
    assert!(
        drift.altered_objects.iter().any(|altered| {
            altered.object == "constraint fk_child_parent"
                && altered.field == "definition"
                && altered.expected.contains("ON UPDATE SET NULL")
                && altered.actual.contains("ON UPDATE CASCADE")
                && altered.expected.contains("INITIALLY DEFERRED")
                && !altered.actual.contains("INITIALLY DEFERRED")
        }),
        "policy and deferrability changes must surface as definition drift: {drift:?}"
    );
}

// ---------------------------------------------------------------------------
// Full reference regression matrix. The single-column fixture uses the same
// physical shape as a typed TypeID reference: the parent owns the format CHECK,
// the child inherits that safety through its FK and therefore has no local
// ValueFormat CHECK. Composite cases prove ordered tuple identity as well as
// target/action identity.
// ---------------------------------------------------------------------------
#[compio::test]
async fn authored_composite_reference_snapshot_matches_live_sqlite() {
    let ir: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "authored_composite_reference",
        "owner_app": "app_sqlite_drift",
        "ops": [
            {
                "op": "createTable",
                "name": "parent",
                "columns": [
                    { "name": "tenant", "type": "text", "nullable": false },
                    { "name": "parent_id", "type": "int", "nullable": false }
                ],
                "primaryKey": ["tenant", "parent_id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "child",
                "columns": [
                    { "name": "tenant", "type": "text", "nullable": true },
                    { "name": "parent_id", "type": "int", "nullable": true }
                ],
                "primaryKey": null,
                "constraints": [{
                    "name": "fk_child_parent",
                    "kind": {
                        "kind": "fk",
                        "columns": ["tenant", "parent_id"],
                        "referencesTable": "parent",
                        "referencesColumns": ["tenant", "parent_id"],
                        "onDelete": "cascade",
                        "onUpdate": "setNull"
                    }
                }],
                "indexes": []
            }
        ]
    }))
    .expect("authored composite-reference fixture must deserialize");
    let mut expected = fold_ops(
        zeroship_migrate::shipping_vendors(),
        &ir.ops,
        &zeroship_migrate_sqlite::DIALECT,
        "main",
        &support::no_inject("app"),
    )
    .expect("authored composite-reference fixture must fold");
    let migrations = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        "main",
        "app_sqlite_drift",
        &zeroship_migrate_sqlite::DIALECT,
        &support::no_inject("main"),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("authored composite-reference fixture must lower");
    let p = paths("authored_composite_reference");
    let be = backend(&p);
    for migration in &migrations {
        be.apply_one_additive(migration, "d")
            .await
            .unwrap_or_else(|error| panic!("apply {}: {error}", migration.name));
    }
    let mut actual = be.snapshot_schema_sqlite().await.expect("live snapshot");
    strip_sqlite_primary_key_catalog_noise(&mut expected);
    strip_sqlite_primary_key_catalog_noise(&mut actual);
    let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual);
    assert!(
        drift.is_clean(),
        "authored composite FK must round-trip through SQLite catalog introspection: {drift:#?}"
    );
}

#[compio::test]
async fn single_and_composite_reference_drop_repoint_reorder_and_actions_drift() {
    let type_id_parent = lower_id_table_sql(
        "parent",
        "text",
        Some("acct"),
        false,
    );
    let type_id_alternate = lower_id_table_sql(
        "alternate_parent",
        "text",
        Some("acct"),
        false,
    );
    let single_parents = format!("{type_id_parent}; {type_id_alternate};");
    let single_expected = "CREATE TABLE child (\
        parent_id TEXT COLLATE BINARY, \
        CONSTRAINT parent_id_fkey \
            FOREIGN KEY (parent_id) REFERENCES parent (id) \
            ON DELETE CASCADE ON UPDATE SET NULL);";
    let single_cases = [
        (
            "single_fk_drop",
            "CREATE TABLE child (parent_id TEXT COLLATE BINARY);",
            true,
            "",
            "",
        ),
        (
            "single_fk_repoint",
            "CREATE TABLE child (\
                parent_id TEXT COLLATE BINARY, \
                CONSTRAINT parent_id_fkey \
                    FOREIGN KEY (parent_id) REFERENCES alternate_parent (id) \
                    ON DELETE CASCADE ON UPDATE SET NULL);",
            false,
            "REFERENCES parent(id)",
            "REFERENCES alternate_parent(id)",
        ),
        (
            "single_fk_action",
            "CREATE TABLE child (\
                parent_id TEXT COLLATE BINARY, \
                CONSTRAINT parent_id_fkey \
                    FOREIGN KEY (parent_id) REFERENCES parent (id) \
                    ON DELETE RESTRICT ON UPDATE CASCADE);",
            false,
            "ON UPDATE SET NULL ON DELETE CASCADE",
            "ON UPDATE CASCADE ON DELETE RESTRICT",
        ),
    ];

    for (tag, changed_child, dropped, expected_fragment, actual_fragment) in single_cases {
        let p = paths(tag);
        let be = backend(&p);
        let setup = format!("{single_parents} {single_expected}");
        be.apply_one_additive(&mig(tag, &setup), "d")
            .await
            .expect("apply single-column typed-reference fixture");
        let expected = be
            .snapshot_schema_sqlite()
            .await
            .expect("expected snapshot");
        let child = &expected.tables["child"];
        assert!(
            child
                .constraints
                .iter()
                .any(|constraint| constraint.name == "parent_id_fkey"),
            "typed child FK must be introspected"
        );
        let clean = be.snapshot_schema_sqlite().await.expect("clean snapshot");
        assert!(
            diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &clean).is_clean(),
            "unchanged single-column FK must stay clean"
        );
        drop(be);

        replace_table(&p, "child", changed_child);
        let actual = backend(&p)
            .snapshot_schema_sqlite()
            .await
            .expect("actual snapshot");
        let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual);
        if dropped {
            assert!(
                drift
                    .missing_objects
                    .iter()
                    .any(|object| object.contains("parent_id_fkey")),
                "dropped single-column FK must be missing drift: {drift:?}"
            );
        } else {
            assert_constraint_definition_drift(
                &drift,
                "parent_id_fkey",
                expected_fragment,
                actual_fragment,
            );
        }
    }

    const COMPOSITE_PARENTS: &str = "\
        CREATE TABLE parent (tenant TEXT NOT NULL, parent_id INTEGER NOT NULL, \
            PRIMARY KEY (tenant, parent_id)); \
        CREATE TABLE alternate_parent (tenant TEXT NOT NULL, parent_id INTEGER NOT NULL, \
            PRIMARY KEY (tenant, parent_id));";
    const COMPOSITE_EXPECTED: &str = "CREATE TABLE child (\
        tenant TEXT, parent_id INTEGER, \
        CONSTRAINT fk_child_parent \
            FOREIGN KEY (tenant, parent_id) REFERENCES parent (tenant, parent_id) \
            ON DELETE CASCADE ON UPDATE SET NULL);";
    let composite_cases = [
        (
            "composite_fk_drop",
            "CREATE TABLE child (tenant TEXT, parent_id INTEGER);",
            true,
            "",
            "",
        ),
        (
            "composite_fk_repoint",
            "CREATE TABLE child (\
                tenant TEXT, parent_id INTEGER, \
                CONSTRAINT fk_child_parent \
                    FOREIGN KEY (tenant, parent_id) \
                    REFERENCES alternate_parent (tenant, parent_id) \
                    ON DELETE CASCADE ON UPDATE SET NULL);",
            false,
            "REFERENCES parent(tenant, parent_id)",
            "REFERENCES alternate_parent(tenant, parent_id)",
        ),
        (
            "composite_fk_reorder",
            "CREATE TABLE child (\
                tenant TEXT, parent_id INTEGER, \
                CONSTRAINT fk_child_parent \
                    FOREIGN KEY (parent_id, tenant) REFERENCES parent (parent_id, tenant) \
                    ON DELETE CASCADE ON UPDATE SET NULL);",
            false,
            "FOREIGN KEY (tenant, parent_id) REFERENCES parent(tenant, parent_id)",
            "FOREIGN KEY (parent_id, tenant) REFERENCES parent(parent_id, tenant)",
        ),
        (
            "composite_fk_action",
            "CREATE TABLE child (\
                tenant TEXT, parent_id INTEGER, \
                CONSTRAINT fk_child_parent \
                    FOREIGN KEY (tenant, parent_id) REFERENCES parent (tenant, parent_id) \
                    ON DELETE RESTRICT ON UPDATE CASCADE);",
            false,
            "ON UPDATE SET NULL ON DELETE CASCADE",
            "ON UPDATE CASCADE ON DELETE RESTRICT",
        ),
    ];

    for (tag, changed_child, dropped, expected_fragment, actual_fragment) in composite_cases {
        let p = paths(tag);
        let be = backend(&p);
        let setup = format!("{COMPOSITE_PARENTS} {COMPOSITE_EXPECTED}");
        be.apply_one_additive(&mig(tag, &setup), "d")
            .await
            .expect("apply composite-reference fixture");
        let expected = be
            .snapshot_schema_sqlite()
            .await
            .expect("expected snapshot");
        let clean = be.snapshot_schema_sqlite().await.expect("clean snapshot");
        assert!(
            diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &clean).is_clean(),
            "unchanged composite FK must stay clean"
        );
        drop(be);

        replace_table(&p, "child", changed_child);
        let actual = backend(&p)
            .snapshot_schema_sqlite()
            .await
            .expect("actual snapshot");
        let drift = diff_snapshots(zeroship_migrate::shipping_vendors(), &expected, &actual);
        if dropped {
            assert!(
                drift
                    .missing_objects
                    .iter()
                    .any(|object| object.contains("fk_child_parent")),
                "dropped composite FK must be missing drift: {drift:?}"
            );
        } else {
            assert_constraint_definition_drift(
                &drift,
                "fk_child_parent",
                expected_fragment,
                actual_fragment,
            );
        }
    }
}
