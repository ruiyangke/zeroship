//! TypeID 0.3 physical-storage and format-validation regressions.
//!
//! SQLite runs in-process on every test invocation. PostgreSQL uses the shared
//! live-test seam and skips cleanly unless `ZERO_MIGRATE_TEST_PG_URL` is set.

mod support;

use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{IrAuthor, LiveSchema, SqlDialect};

const VALID_BARE: &[&str] = &[
    "00000000000000000000000000",
    "00000000000000000000000001",
    "0000000000000000000000000a",
    "0000000000000000000000000g",
    "00000000000000000000000010",
    "7zzzzzzzzzzzzzzzzzzzzzzzzz",
];

const VALID_PREFIXED: &[(&str, &str)] = &[
    ("prefix", "prefix_0123456789abcdefghjkmnpqrs"),
    ("prefix", "prefix_01h455vb4pex5vsknk084sn02q"),
    ("pre_fix", "pre_fix_00000000000000000000000000"),
];

const INVALID_OFFICIAL: &[&str] = &[
    "PREFIX_00000000000000000000000000",
    "12345_00000000000000000000000000",
    "pre.fix_00000000000000000000000000",
    "préfix_00000000000000000000000000",
    " prefix_00000000000000000000000000",
    "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijkl_00000000000000000000000000",
    "_00000000000000000000000000",
    "_",
    "prefix_1234567890123456789012345",
    "prefix_123456789012345678901234567",
    "prefix_1234567890123456789012345 ",
    "prefix_0123456789ABCDEFGHJKMNPQRS",
    "prefix_123456789-123456789-123456",
    "prefix_ooooooiiiiiiuuuuuuulllllll",
    "prefix_i23456789ol23456789oi23456",
    "prefix_123456789-0123456789-0123456",
    "prefix_8zzzzzzzzzzzzzzzzzzzzzzzzz",
    "_prefix_00000000000000000000000000",
    "prefix__00000000000000000000000000",
    "",
    "prefix_",
];

fn type_id_ir(table: &str, prefix: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": format!("create_{table}"),
        "owner_app": "app_type_id_samples",
        "ops": [{
            "op": "createTable",
            "name": table,
            "columns": [{
                "name": "id",
                "type": "text",
                "nullable": true,
                "valueFormat": { "typeId": { "prefix": prefix } }
            }],
            "primaryKey": null,
            "indexes": []
        }]
    }))
    .expect("TypeID create-table IR must deserialize")
}

fn lower_create(dialect: SqlDialect, table: &str, prefix: &str) -> String {
    let migrations = IrAuthor::new(
        "app",
        "app_type_id_samples",
        dialect,
        &support::no_inject("app"),
    )
    .lower(&type_id_ir(table, prefix), &LiveSchema::default())
    .expect("TypeID create table must lower");
    assert_eq!(migrations.len(), 1);
    migrations.into_iter().next().unwrap().up
}

fn lower_add(dialect: SqlDialect, table: &str, prefix: &str) -> String {
    let ir: MigrationIr = serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": format!("add_{table}_id"),
        "owner_app": "app_type_id_samples",
        "ops": [{
            "op": "addColumn",
            "table": table,
            "column": "public_id",
            "type": "text",
            "valueFormat": { "typeId": { "prefix": prefix } }
        }]
    }))
    .expect("TypeID add-column IR must deserialize");
    let migrations = IrAuthor::new(
        "app",
        "app_type_id_samples",
        dialect,
        &support::no_inject("app"),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("TypeID add column must lower");
    assert_eq!(migrations.len(), 1);
    migrations.into_iter().next().unwrap().up
}

#[test]
fn type_id_create_table_ddl_is_exact_on_all_dialects() {
    assert_eq!(
        lower_create(SqlDialect::Postgres, "type_ids", "prefix"),
        "CREATE TABLE \"app\".\"type_ids\" (\"id\" text COLLATE \"C\" CHECK (\"id\" IS NULL OR (octet_length(\"id\") = 33 AND (\"id\" COLLATE \"C\") ~ '^prefix_[0-7][0123456789abcdefghjkmnpqrstvwxyz]{25}$')))"
    );
    assert_eq!(
        lower_create(SqlDialect::Mysql, "type_ids", "prefix"),
        "CREATE TABLE `app`.`type_ids` (`id` VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin CHECK (`id` IS NULL OR (CHAR_LENGTH(`id`) = 33 AND REGEXP_LIKE(`id`, '^prefix_[0-7][0123456789abcdefghjkmnpqrstvwxyz]{25}$', 'c'))))"
    );
    assert_eq!(
        lower_create(SqlDialect::Sqlite, "type_ids", "prefix"),
        "CREATE TABLE \"type_ids\" (\"id\" TEXT COLLATE BINARY CHECK (\"id\" IS NULL OR (typeof(\"id\") = 'text' AND length(\"id\") = 33 AND length(CAST(\"id\" AS BLOB)) = 33 AND substr(\"id\", 1, 7) = 'prefix_' COLLATE BINARY AND substr(\"id\", 8, 1) GLOB '[0-7]' AND substr(\"id\", 8, 26) NOT GLOB '*[^0123456789abcdefghjkmnpqrstvwxyz]*')))"
    );

    assert_eq!(
        lower_create(SqlDialect::Postgres, "bare_type_ids", ""),
        "CREATE TABLE \"app\".\"bare_type_ids\" (\"id\" text COLLATE \"C\" CHECK (\"id\" IS NULL OR (octet_length(\"id\") = 26 AND (\"id\" COLLATE \"C\") ~ '^[0-7][0123456789abcdefghjkmnpqrstvwxyz]{25}$')))"
    );
    assert_eq!(
        lower_create(SqlDialect::Mysql, "bare_type_ids", ""),
        "CREATE TABLE `app`.`bare_type_ids` (`id` VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin CHECK (`id` IS NULL OR (CHAR_LENGTH(`id`) = 26 AND REGEXP_LIKE(`id`, '^[0-7][0123456789abcdefghjkmnpqrstvwxyz]{25}$', 'c'))))"
    );
    assert_eq!(
        lower_create(SqlDialect::Sqlite, "bare_type_ids", ""),
        "CREATE TABLE \"bare_type_ids\" (\"id\" TEXT COLLATE BINARY CHECK (\"id\" IS NULL OR (typeof(\"id\") = 'text' AND length(\"id\") = 26 AND length(CAST(\"id\" AS BLOB)) = 26 AND substr(\"id\", 1, 1) GLOB '[0-7]' AND substr(\"id\", 1, 26) NOT GLOB '*[^0123456789abcdefghjkmnpqrstvwxyz]*')))"
    );
}

#[test]
fn type_id_add_column_ddl_keeps_the_same_storage_and_check() {
    assert_eq!(
        lower_add(SqlDialect::Postgres, "type_ids", "prefix"),
        "ALTER TABLE \"app\".\"type_ids\" ADD COLUMN \"public_id\" text COLLATE \"C\" CHECK (\"public_id\" IS NULL OR (octet_length(\"public_id\") = 33 AND (\"public_id\" COLLATE \"C\") ~ '^prefix_[0-7][0123456789abcdefghjkmnpqrstvwxyz]{25}$'))"
    );
    assert_eq!(
        lower_add(SqlDialect::Mysql, "type_ids", "prefix"),
        "ALTER TABLE `app`.`type_ids` ADD COLUMN `public_id` VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin CHECK (`public_id` IS NULL OR (CHAR_LENGTH(`public_id`) = 33 AND REGEXP_LIKE(`public_id`, '^prefix_[0-7][0123456789abcdefghjkmnpqrstvwxyz]{25}$', 'c')))"
    );
    assert_eq!(
        lower_add(SqlDialect::Sqlite, "type_ids", "prefix"),
        "ALTER TABLE \"type_ids\" ADD COLUMN \"public_id\" TEXT COLLATE BINARY CHECK (\"public_id\" IS NULL OR (typeof(\"public_id\") = 'text' AND length(\"public_id\") = 33 AND length(CAST(\"public_id\" AS BLOB)) = 33 AND substr(\"public_id\", 1, 7) = 'prefix_' COLLATE BINARY AND substr(\"public_id\", 8, 1) GLOB '[0-7]' AND substr(\"public_id\", 8, 26) NOT GLOB '*[^0123456789abcdefghjkmnpqrstvwxyz]*'))"
    );
}

#[test]
fn sqlite_enforces_official_type_id_fixtures_and_text_storage() {
    let conn = rusqlite::Connection::open_in_memory().expect("open SQLite");
    for (table, prefix) in [("bare", ""), ("prefixed", "prefix"), ("split", "pre_fix")] {
        conn.execute_batch(&lower_create(SqlDialect::Sqlite, table, prefix))
            .unwrap_or_else(|error| panic!("apply TypeID table {table}: {error}"));
    }

    for value in VALID_BARE {
        conn.execute("INSERT INTO bare(id) VALUES (?1)", [value])
            .unwrap_or_else(|error| panic!("valid bare TypeID {value:?}: {error}"));
    }
    for (prefix, value) in VALID_PREFIXED {
        let table = if *prefix == "pre_fix" {
            "split"
        } else {
            "prefixed"
        };
        conn.execute(&format!("INSERT INTO {table}(id) VALUES (?1)"), [value])
            .unwrap_or_else(|error| panic!("valid prefixed TypeID {value:?}: {error}"));
    }
    conn.execute("INSERT INTO prefixed(id) VALUES (NULL)", [])
        .expect("TypeID CHECK must remain null-tolerant");

    for value in INVALID_OFFICIAL {
        assert!(
            conn.execute("INSERT INTO prefixed(id) VALUES (?1)", [value])
                .is_err(),
            "invalid official TypeID fixture was accepted: {value:?}"
        );
    }

    assert!(
        conn.execute(
            "INSERT INTO prefixed(id) VALUES (?1)",
            [rusqlite::types::Value::Blob(
                b"prefix_00000000000000000000000000".to_vec()
            )],
        )
        .is_err(),
        "SQLite TypeID validation must reject canonical-looking BLOB bytes"
    );

    let nul_suffixed = "prefix_00000000000000000000000000\0junk";
    assert!(
        conn.execute("INSERT INTO prefixed(id) VALUES (?1)", [nul_suffixed])
            .is_err(),
        "SQLite TypeID validation must reject canonical text followed by embedded-NUL bytes"
    );
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    format!("{}_{}_{}", std::process::id(), nanos, n)
}

fn pg_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[compio::test]
async fn postgres_enforces_official_type_id_fixtures() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("type_id_{}", token());
    session
        .batch(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .expect("create TypeID fixture schema");

    async {
        for (table, prefix) in [("bare", ""), ("prefixed", "prefix"), ("split", "pre_fix")] {
            let ir = type_id_ir(table, prefix);
            let migrations = IrAuthor::new(
                &schema,
                "app_type_id_samples",
                SqlDialect::Postgres,
                &support::no_inject(&schema),
            )
            .lower(&ir, &LiveSchema::default())
            .expect("lower PostgreSQL TypeID table");
            assert_eq!(migrations.len(), 1);
            session
                .batch(&migrations[0].up)
                .await
                .unwrap_or_else(|error| panic!("apply TypeID table {table}: {error}"));
        }

        for value in VALID_BARE {
            session
                .batch(&format!(
                    "INSERT INTO \"{schema}\".\"bare\" (id) VALUES ({})",
                    pg_literal(value)
                ))
                .await
                .unwrap_or_else(|error| panic!("valid bare TypeID {value:?}: {error}"));
        }
        for (prefix, value) in VALID_PREFIXED {
            let table = if *prefix == "pre_fix" {
                "split"
            } else {
                "prefixed"
            };
            session
                .batch(&format!(
                    "INSERT INTO \"{schema}\".\"{table}\" (id) VALUES ({})",
                    pg_literal(value)
                ))
                .await
                .unwrap_or_else(|error| panic!("valid prefixed TypeID {value:?}: {error}"));
        }
        session
            .batch(&format!(
                "INSERT INTO \"{schema}\".\"prefixed\" (id) VALUES (NULL)"
            ))
            .await
            .expect("TypeID CHECK must remain null-tolerant");

        for value in INVALID_OFFICIAL {
            let result = session
                .batch(&format!(
                    "INSERT INTO \"{schema}\".\"prefixed\" (id) VALUES ({})",
                    pg_literal(value)
                ))
                .await;
            assert!(
                result.is_err(),
                "invalid official TypeID fixture was accepted: {value:?}"
            );
        }
    }
    .await;

    let _ = session
        .batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
        .await;
}
