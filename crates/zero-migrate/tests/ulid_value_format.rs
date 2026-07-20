//! ULID physical-storage and format-validation regressions.
//!
//! SQLite runs in-process on every test invocation. PostgreSQL uses the shared
//! live-test seam and skips cleanly unless `ZERO_MIGRATE_TEST_PG_URL` is set.

mod support;

use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{IrAuthor, LiveSchema, SqlDialect};

const VALID_ULIDS_IN_BYTEWISE_ORDER: &[&str] = &[
    "00000000000000000000000000",
    "00000000000000000000000001",
    "0000000000000000000000000A",
    "0000000000000000000000000Z",
    "00000000000000000000000010",
    "0123456789ABCDEFGHJKMNPQRS",
    "01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "7ZZZZZZZZZZZZZZZZZZZZZZZZZ",
];

const INVALID_ULIDS: &[&str] = &[
    // Exact length is 26 characters and 26 UTF-8 bytes.
    "",
    "0000000000000000000000000",
    "000000000000000000000000000",
    "01ARZ3NDEKTSV4RRFFQ69G5FAV ",
    "01ARZ3NDEKTSV4RRFFQ69G5FAé",
    // Crockford Base32 excludes I, L, O, and U and admits no punctuation.
    "01ARZ3NDEKTSV4RRFFQ69G5FAI",
    "01ARZ3NDEKTSV4RRFFQ69G5FAL",
    "01ARZ3NDEKTSV4RRFFQ69G5FAO",
    "01ARZ3NDEKTSV4RRFFQ69G5FAU",
    "01ARZ3NDEKTSV4RRFFQ69G5FA-",
    // The stored spelling is canonical uppercase; writers are not normalized.
    "01arz3ndektsv4rrffq69g5fav",
    "01ARZ3NDEKTSV4RRFFQ69G5FAv",
    // A 26-character Base32 string beginning above 7 exceeds 128 bits.
    "80000000000000000000000000",
    "ZZZZZZZZZZZZZZZZZZZZZZZZZZ",
];

const UPPERCASE_CASE_FIXTURE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const LOWERCASE_CASE_FIXTURE: &str = "01arz3ndektsv4rrffq69g5fav";

fn ulid_ir(table: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": format!("create_{table}"),
        "owner_app": "app_ulid_samples",
        "ops": [{
            "op": "createTable",
            "name": table,
            "columns": [{
                "name": "id",
                "type": "text",
                "nullable": true,
                "valueFormat": "ulid"
            }],
            "primaryKey": null,
            "indexes": []
        }]
    }))
    .expect("ULID create-table IR must deserialize")
}

fn bare_type_id_ir(table: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": format!("create_{table}"),
        "owner_app": "app_ulid_samples",
        "ops": [{
            "op": "createTable",
            "name": table,
            "columns": [{
                "name": "id",
                "type": "text",
                "nullable": true,
                "valueFormat": { "typeId": { "prefix": "" } }
            }],
            "primaryKey": null,
            "indexes": []
        }]
    }))
    .expect("bare TypeID create-table IR must deserialize")
}

fn lower_create_for_schema(dialect: SqlDialect, schema: &str, ir: &MigrationIr) -> String {
    let migrations = IrAuthor::new(
        schema,
        "app_ulid_samples",
        dialect,
        &support::no_inject("app"),
    )
    .lower(ir, &LiveSchema::default())
    .expect("value-format create table must lower");
    assert_eq!(migrations.len(), 1);
    migrations.into_iter().next().unwrap().up
}

fn lower_create(dialect: SqlDialect, table: &str) -> String {
    lower_create_for_schema(dialect, "app", &ulid_ir(table))
}

fn lower_add(dialect: SqlDialect, table: &str) -> String {
    let ir: MigrationIr = serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": format!("add_{table}_id"),
        "owner_app": "app_ulid_samples",
        "ops": [{
            "op": "addColumn",
            "table": table,
            "column": "public_id",
            "type": "text",
            "valueFormat": "ulid"
        }]
    }))
    .expect("ULID add-column IR must deserialize");
    let migrations = IrAuthor::new(
        "app",
        "app_ulid_samples",
        dialect,
        &support::no_inject("app"),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("ULID add column must lower");
    assert_eq!(migrations.len(), 1);
    migrations.into_iter().next().unwrap().up
}

#[test]
fn ulid_create_table_ddl_is_exact_on_all_dialects() {
    assert_eq!(
        lower_create(SqlDialect::Postgres, "ulids"),
        "CREATE TABLE \"app\".\"ulids\" (\"id\" text COLLATE \"C\" CHECK (\"id\" IS NULL OR (octet_length(\"id\") = 26 AND (\"id\" COLLATE \"C\") ~ '^[0-7][0123456789ABCDEFGHJKMNPQRSTVWXYZ]{25}$')))"
    );
    assert_eq!(
        lower_create(SqlDialect::Mysql, "ulids"),
        "CREATE TABLE `app`.`ulids` (`id` VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin CHECK (`id` IS NULL OR (CHAR_LENGTH(`id`) = 26 AND REGEXP_LIKE(`id`, '^[0-7][0123456789ABCDEFGHJKMNPQRSTVWXYZ]{25}$', 'c'))))"
    );
    assert_eq!(
        lower_create(SqlDialect::Sqlite, "ulids"),
        "CREATE TABLE \"ulids\" (\"id\" TEXT COLLATE BINARY CHECK (\"id\" IS NULL OR (typeof(\"id\") = 'text' AND length(\"id\") = 26 AND length(CAST(\"id\" AS BLOB)) = 26 AND substr(\"id\", 1, 1) GLOB '[0-7]' AND substr(\"id\", 1, 26) NOT GLOB '*[^0123456789ABCDEFGHJKMNPQRSTVWXYZ]*')))"
    );
}

#[test]
fn ulid_add_column_ddl_keeps_the_same_storage_and_check() {
    assert_eq!(
        lower_add(SqlDialect::Postgres, "ulids"),
        "ALTER TABLE \"app\".\"ulids\" ADD COLUMN \"public_id\" text COLLATE \"C\" CHECK (\"public_id\" IS NULL OR (octet_length(\"public_id\") = 26 AND (\"public_id\" COLLATE \"C\") ~ '^[0-7][0123456789ABCDEFGHJKMNPQRSTVWXYZ]{25}$'))"
    );
    assert_eq!(
        lower_add(SqlDialect::Mysql, "ulids"),
        "ALTER TABLE `app`.`ulids` ADD COLUMN `public_id` VARCHAR(191) CHARACTER SET ascii COLLATE ascii_bin CHECK (`public_id` IS NULL OR (CHAR_LENGTH(`public_id`) = 26 AND REGEXP_LIKE(`public_id`, '^[0-7][0123456789ABCDEFGHJKMNPQRSTVWXYZ]{25}$', 'c')))"
    );
    assert_eq!(
        lower_add(SqlDialect::Sqlite, "ulids"),
        "ALTER TABLE \"ulids\" ADD COLUMN \"public_id\" TEXT COLLATE BINARY CHECK (\"public_id\" IS NULL OR (typeof(\"public_id\") = 'text' AND length(\"public_id\") = 26 AND length(CAST(\"public_id\" AS BLOB)) = 26 AND substr(\"public_id\", 1, 1) GLOB '[0-7]' AND substr(\"public_id\", 1, 26) NOT GLOB '*[^0123456789ABCDEFGHJKMNPQRSTVWXYZ]*'))"
    );
}

#[test]
fn sqlite_enforces_ulid_spelling_storage_and_bytewise_order() {
    let conn = rusqlite::Connection::open_in_memory().expect("open SQLite");
    conn.execute_batch(&lower_create(SqlDialect::Sqlite, "ulids"))
        .expect("apply SQLite ULID table");

    for value in VALID_ULIDS_IN_BYTEWISE_ORDER.iter().rev() {
        conn.execute("INSERT INTO ulids(id) VALUES (?1)", [value])
            .unwrap_or_else(|error| panic!("valid ULID {value:?}: {error}"));
    }
    conn.execute("INSERT INTO ulids(id) VALUES (NULL)", [])
        .expect("ULID CHECK must remain null-tolerant");

    for value in INVALID_ULIDS {
        assert!(
            conn.execute("INSERT INTO ulids(id) VALUES (?1)", [value])
                .is_err(),
            "invalid ULID fixture was accepted: {value:?}"
        );
    }

    assert!(
        conn.execute(
            "INSERT INTO ulids(id) VALUES (?1)",
            [rusqlite::types::Value::Blob(
                b"01ARZ3NDEKTSV4RRFFQ69G5FAV".to_vec()
            )],
        )
        .is_err(),
        "SQLite ULID validation must reject canonical-looking BLOB bytes"
    );

    let nul_suffixed = "01ARZ3NDEKTSV4RRFFQ69G5FAV\0junk";
    assert!(
        conn.execute("INSERT INTO ulids(id) VALUES (?1)", [nul_suffixed])
            .is_err(),
        "SQLite ULID validation must reject canonical text followed by embedded-NUL bytes"
    );

    let mut statement = conn
        .prepare("SELECT id FROM ulids WHERE id IS NOT NULL ORDER BY id")
        .expect("prepare bytewise ULID ordering query");
    let ordered = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query ordered ULIDs")
        .collect::<Result<Vec<_>, _>>()
        .expect("decode ordered ULIDs");
    let expected = VALID_ULIDS_IN_BYTEWISE_ORDER
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        ordered, expected,
        "SQLite BINARY collation must preserve canonical ULID lexical/time order"
    );
}

#[test]
fn sqlite_ulid_and_empty_prefix_type_id_checks_are_case_distinct() {
    let conn = rusqlite::Connection::open_in_memory().expect("open SQLite");
    conn.execute_batch(&lower_create(SqlDialect::Sqlite, "ulids"))
        .expect("apply SQLite ULID table");
    conn.execute_batch(&lower_create_for_schema(
        SqlDialect::Sqlite,
        "app",
        &bare_type_id_ir("type_ids"),
    ))
    .expect("apply SQLite bare TypeID table");

    conn.execute(
        "INSERT INTO ulids(id) VALUES (?1)",
        [UPPERCASE_CASE_FIXTURE],
    )
    .expect("canonical uppercase ULID must pass the ULID check");
    assert!(
        conn.execute(
            "INSERT INTO ulids(id) VALUES (?1)",
            [LOWERCASE_CASE_FIXTURE],
        )
        .is_err(),
        "the equivalent lowercase TypeID spelling must fail the ULID check"
    );

    conn.execute(
        "INSERT INTO type_ids(id) VALUES (?1)",
        [LOWERCASE_CASE_FIXTURE],
    )
    .expect("canonical lowercase empty-prefix TypeID must pass its check");
    assert!(
        conn.execute(
            "INSERT INTO type_ids(id) VALUES (?1)",
            [UPPERCASE_CASE_FIXTURE],
        )
        .is_err(),
        "the equivalent uppercase ULID spelling must fail the TypeID check"
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
async fn postgres_enforces_ulid_fixtures_order_and_case_distinction() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("ulid_{}", token());
    session
        .batch(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .expect("create ULID fixture schema");

    async {
        for ir in [ulid_ir("ulids"), bare_type_id_ir("type_ids")] {
            let ddl = lower_create_for_schema(SqlDialect::Postgres, &schema, &ir);
            session
                .batch(&ddl)
                .await
                .unwrap_or_else(|error| panic!("apply value-format fixture table: {error}"));
        }

        for value in VALID_ULIDS_IN_BYTEWISE_ORDER.iter().rev() {
            session
                .batch(&format!(
                    "INSERT INTO \"{schema}\".\"ulids\" (id) VALUES ({})",
                    pg_literal(value)
                ))
                .await
                .unwrap_or_else(|error| panic!("valid ULID {value:?}: {error}"));
        }
        session
            .batch(&format!(
                "INSERT INTO \"{schema}\".\"ulids\" (id) VALUES (NULL)"
            ))
            .await
            .expect("ULID CHECK must remain null-tolerant");

        for value in INVALID_ULIDS {
            let result = session
                .batch(&format!(
                    "INSERT INTO \"{schema}\".\"ulids\" (id) VALUES ({})",
                    pg_literal(value)
                ))
                .await;
            assert!(
                result.is_err(),
                "invalid ULID fixture was accepted: {value:?}"
            );
        }

        let rows = session
            .query(
                &format!(
                    "SELECT id FROM \"{schema}\".\"ulids\" \
                     WHERE id IS NOT NULL ORDER BY id"
                ),
                &[],
            )
            .await
            .expect("query bytewise-ordered PostgreSQL ULIDs");
        let ordered = rows
            .iter()
            .map(|row| row.try_get::<_, String>("id").expect("decode ULID"))
            .collect::<Vec<_>>();
        let expected = VALID_ULIDS_IN_BYTEWISE_ORDER
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            ordered, expected,
            "PostgreSQL C collation must preserve canonical ULID lexical/time order"
        );

        session
            .batch(&format!(
                "INSERT INTO \"{schema}\".\"type_ids\" (id) VALUES ({})",
                pg_literal(LOWERCASE_CASE_FIXTURE)
            ))
            .await
            .expect("canonical lowercase empty-prefix TypeID must pass its check");
        assert!(
            session
                .batch(&format!(
                    "INSERT INTO \"{schema}\".\"type_ids\" (id) VALUES ({})",
                    pg_literal(UPPERCASE_CASE_FIXTURE)
                ))
                .await
                .is_err(),
            "the uppercase ULID spelling must fail the empty-prefix TypeID check"
        );
    }
    .await;

    let _ = session
        .batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
        .await;
}
