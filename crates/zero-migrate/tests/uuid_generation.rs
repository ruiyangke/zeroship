//! Exact database-generated UUID regression tests.
//!
//! SQLite runs in-process on every test invocation. PostgreSQL uses the shared
//! live-test seam and skips cleanly unless `ZERO_MIGRATE_TEST_PG_URL` is set.

mod support;

use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{IrAuthor, LiveSchema, SqlDialect};

fn uuid_v4_ir(table: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": format!("create_{table}"),
        "owner_app": "app_uuid_samples",
        "ops": [{
            "op": "createTable",
            "name": table,
            "columns": [{
                "name": "id",
                "type": "uuid",
                "nullable": false,
                "default": { "expr": { "node": "uuidV4" } }
            }],
            "primaryKey": null,
            "indexes": []
        }]
    }))
    .expect("UUIDv4 create-table IR must deserialize")
}

fn assert_exact_uuid_v4(value: &str) {
    let bytes = value.as_bytes();
    assert_eq!(bytes.len(), 36, "canonical UUID length: {value}");
    for separator in [8, 13, 18, 23] {
        assert_eq!(bytes[separator], b'-', "canonical UUID separators: {value}");
    }
    assert_eq!(bytes[14], b'4', "UUID version bits must be 0100: {value}");
    assert!(
        matches!(bytes[19], b'8' | b'9' | b'a' | b'b'),
        "UUID variant bits must be 10: {value}"
    );
    assert_eq!(value, value.to_ascii_lowercase(), "UUID must be lowercase");
    assert!(
        bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 8 | 13 | 18 | 23)
                || byte.is_ascii_digit()
                || matches!(byte, b'a'..=b'f')
        }),
        "UUID must contain only lowercase hexadecimal digits and separators: {value}"
    );
}

#[test]
fn sqlite_uuid_v4_default_generates_exact_rfc_9562_values() {
    let table = "uuid_v4_samples";
    let ir = uuid_v4_ir(table);
    let migrations = IrAuthor::new(
        "main",
        "app_uuid_samples",
        SqlDialect::Sqlite,
        &zero_migrate::zeroship_no_inject_ceiling(),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("lower SQLite UUIDv4 default");
    assert_eq!(migrations.len(), 1);
    assert!(
        migrations[0].up.contains("randomblob"),
        "SQLite UUIDv4 default must be database-generated: {}",
        migrations[0].up
    );

    let conn = rusqlite::Connection::open_in_memory().expect("open SQLite");
    conn.execute_batch(&migrations[0].up)
        .expect("apply rendered SQLite create table");
    for _ in 0..128 {
        conn.execute(&format!("INSERT INTO \"{table}\" DEFAULT VALUES"), [])
            .expect("insert through UUIDv4 default");
    }
    let mut stmt = conn
        .prepare(&format!("SELECT id FROM \"{table}\""))
        .expect("prepare UUID sample query");
    let values = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query UUID samples");
    for value in values {
        assert_exact_uuid_v4(&value.expect("decode SQLite UUID text"));
    }
}

#[test]
fn mysql_uuid_v4_default_uses_exact_random_bytes_expression() {
    let ir = uuid_v4_ir("uuid_v4_samples");
    let migrations = IrAuthor::new(
        "app",
        "app_uuid_samples",
        SqlDialect::Mysql,
        &zero_migrate::zeroship_no_inject_ceiling(),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("lower MySQL UUIDv4 default");
    assert_eq!(migrations.len(), 1);
    let sql = &migrations[0].up;

    assert!(
        sql.contains("DEFAULT (lower(concat(hex(random_bytes(4))"),
        "MySQL UUIDv4 default must synthesize canonical text from random bytes: {sql}"
    );
    assert!(
        sql.contains("hex((ord(random_bytes(1)) & 15) | 64)"),
        "MySQL UUIDv4 default must set version bits to 0100: {sql}"
    );
    assert!(
        sql.contains("hex((ord(random_bytes(1)) & 63) | 128)"),
        "MySQL UUIDv4 default must set RFC variant bits to 10: {sql}"
    );
    assert!(
        !sql.contains("UUID()"),
        "MySQL UUID() is UUIDv1 and must never render for uuidV4(): {sql}"
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

#[compio::test]
async fn postgres_uuid_v4_default_generates_exact_rfc_9562_values() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("uuid_v4_{}", token());
    let table = "samples";
    session
        .batch(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .expect("create UUID sample schema");

    async {
        let ir = uuid_v4_ir(table);
        let migrations = IrAuthor::new(
            &schema,
            "app_uuid_samples",
            SqlDialect::Postgres,
            &zero_migrate::confined_no_inject_policy(&schema)
                .expect("UUID no-inject policy composes"),
        )
        .lower(&ir, &LiveSchema::default())
        .expect("lower PostgreSQL UUIDv4 default");
        assert_eq!(migrations.len(), 1);
        assert!(
            migrations[0].up.contains("DEFAULT gen_random_uuid()"),
            "PostgreSQL 13+ must use the core UUIDv4 generator: {}",
            migrations[0].up
        );
        session
            .batch(&migrations[0].up)
            .await
            .expect("apply rendered PostgreSQL create table");
        for _ in 0..128 {
            session
                .batch(&format!(
                    "INSERT INTO \"{schema}\".\"{table}\" DEFAULT VALUES"
                ))
                .await
                .expect("insert through PostgreSQL UUIDv4 default");
        }
        let rows = session
            .query(
                &format!("SELECT id::text AS id FROM \"{schema}\".\"{table}\" ORDER BY id"),
                &[],
            )
            .await
            .expect("query PostgreSQL UUID samples");
        assert_eq!(rows.len(), 128);
        for row in rows {
            let value: String = row.try_get("id").expect("decode PostgreSQL UUID text");
            assert_exact_uuid_v4(&value);
        }
    }
    .await;

    let _ = session
        .batch(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
        .await;
}
