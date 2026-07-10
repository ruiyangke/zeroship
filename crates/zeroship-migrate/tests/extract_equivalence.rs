#![cfg(feature = "zsv8")]
#![allow(unsafe_code)]

#[path = "../../runtime/tests/support/node_realworld.rs"]
mod node_realworld;

use std::time::Duration;

use compio_postgres::Client;
use serde_json::{json, Value};
use zeroship_migrate::{render::step::BindValue, MysqlBackend, RowSet};

use node_realworld::{allowlist, ensure_mysql, lock_env, EnvGuard, LOCALHOST};

const PG_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";
const MYSQL_DSN: &str = "mysql://root:zeroship@127.0.0.1:3307/zeroship_e2e";

#[derive(Debug, Clone)]
struct MysqlDsn {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

impl MysqlDsn {
    fn json(&self) -> String {
        json!({
            "host": self.host,
            "port": self.port,
            "user": self.user,
            "password": self.password,
            "database": self.database,
        })
        .to_string()
    }
}

fn pg_dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| PG_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&pg_dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn configured_mysql_dsn() -> String {
    std::env::var("MYSQL_JS_DRIVER_E2E_DSN").unwrap_or_else(|_| MYSQL_DSN.to_string())
}

fn parse_mysql_dsn(raw: &str) -> MysqlDsn {
    let rest = raw
        .strip_prefix("mysql://")
        .unwrap_or_else(|| panic!("MYSQL_JS_DRIVER_E2E_DSN must start with mysql://, got {raw:?}"));
    let (auth_host, database) = rest
        .split_once('/')
        .unwrap_or_else(|| panic!("MYSQL_JS_DRIVER_E2E_DSN must include a database, got {raw:?}"));
    let (auth, host_port) = auth_host
        .rsplit_once('@')
        .unwrap_or_else(|| panic!("MYSQL_JS_DRIVER_E2E_DSN must include user info, got {raw:?}"));
    let (user, password) = auth
        .split_once(':')
        .unwrap_or_else(|| panic!("MYSQL_JS_DRIVER_E2E_DSN must include user:password, got {raw:?}"));
    let (host, port) = host_port.rsplit_once(':').unwrap_or((host_port, "3306"));
    MysqlDsn {
        host: host.to_string(),
        port: port
            .parse()
            .unwrap_or_else(|_| panic!("MYSQL_JS_DRIVER_E2E_DSN has invalid port {port:?}")),
        user: user.to_string(),
        password: password.to_string(),
        database: database.to_string(),
    }
}

fn live_mysql_or_skip() -> Option<MysqlDsn> {
    let raw = configured_mysql_dsn();
    let dsn = parse_mysql_dsn(&raw);
    let require = std::env::var("MIGRATE_REQUIRE_MYSQL").is_ok_and(|v| v == "1");
    if raw == MYSQL_DSN {
        match std::panic::catch_unwind(ensure_mysql) {
            Ok(server) => {
                println!(
                    "extract_equivalence connected to MySQL {}:{} via {}",
                    server.host, server.port, server.source
                );
                Some(dsn)
            }
            Err(_) => {
                let message = format!(
                    "default MySQL {LOCALHOST}:3307 unreachable and ensure_mysql() could not start it"
                );
                if require {
                    panic!("MIGRATE_REQUIRE_MYSQL=1: {message}");
                }
                eprintln!("SKIPPED (NOT RUN) extract_equivalence: {message}");
                None
            }
        }
    } else if node_realworld::tcp_reachable(&dsn.host, dsn.port) {
        Some(dsn)
    } else {
        let message = format!(
            "MYSQL_JS_DRIVER_E2E_DSN target {}:{} is unreachable",
            dsn.host, dsn.port
        );
        if require {
            panic!("MIGRATE_REQUIRE_MYSQL=1: {message}");
        }
        eprintln!("SKIPPED (NOT RUN) extract_equivalence: {message}");
        None
    }
}

fn mysql_backend(dsn: &MysqlDsn) -> MysqlBackend {
    MysqlBackend::open_mysql_dsn_json_with_policy(
        dsn.json(),
        allowlist(&dsn.host, dsn.port, 8, 16 * 1024 * 1024),
        Duration::from_secs(45),
    )
    .expect("open live mysql2 JS driver backend")
}

async fn mysql_query(backend: &MysqlBackend, sql: &str) -> RowSet {
    backend
        .query_json(sql, &[] as &[BindValue])
        .await
        .unwrap_or_else(|err| panic!("query failed: {err}: {sql}"))
}

fn json_i64(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|v| i64::try_from(v).ok()))
            .unwrap_or_else(|| panic!("numeric value is not an i64: {value}")),
        Value::String(s) => s
            .parse::<i64>()
            .unwrap_or_else(|err| panic!("string value is not an i64 ({s:?}): {err}")),
        other => panic!("value is not an i64-compatible scalar: {other}"),
    }
}

fn json_f64(value: &Value) -> f64 {
    match value {
        Value::Number(n) => n
            .as_f64()
            .unwrap_or_else(|| panic!("numeric value is not an f64: {value}")),
        Value::String(s) => s
            .parse::<f64>()
            .unwrap_or_else(|err| panic!("string value is not an f64 ({s:?}): {err}")),
        other => panic!("value is not an f64-compatible scalar: {other}"),
    }
}

#[compio::test]
async fn portable_extract_fields_are_live_equivalent_on_all_three_dialects() {
    let Some(mysql_dsn) = live_mysql_or_skip() else { return };
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let mysql = mysql_backend(&mysql_dsn);
    let pg = pg().await;

    let pg_row = pg
        .query_one(
            "SELECT \
                EXTRACT(year FROM TIMESTAMP '2026-07-05 13:14:15')::bigint AS year_part, \
                EXTRACT(month FROM TIMESTAMP '2026-07-05 13:14:15')::bigint AS month_part, \
                EXTRACT(day FROM TIMESTAMP '2026-07-05 13:14:15')::bigint AS day_part, \
                EXTRACT(hour FROM TIMESTAMP '2026-07-05 13:14:15')::bigint AS hour_part, \
                EXTRACT(minute FROM TIMESTAMP '2026-07-05 13:14:15')::bigint AS minute_part, \
                EXTRACT(dow FROM TIMESTAMP '2026-07-05 13:14:15')::bigint AS dow_part, \
                EXTRACT(second FROM TIMESTAMP '2026-07-05 13:14:15.789')::double precision AS second_part",
            &[],
        )
        .await
        .expect("pg extract query");
    let pg_values = [
        pg_row.get::<_, i64>("year_part"),
        pg_row.get::<_, i64>("month_part"),
        pg_row.get::<_, i64>("day_part"),
        pg_row.get::<_, i64>("hour_part"),
        pg_row.get::<_, i64>("minute_part"),
        pg_row.get::<_, i64>("dow_part"),
    ];
    let pg_second = pg_row.get::<_, f64>("second_part");

    let sqlite = rusqlite::Connection::open_in_memory().expect("open sqlite");
    let sqlite_values: [i64; 6] = sqlite
        .query_row(
            "SELECT \
                CAST(strftime('%Y', '2026-07-05 13:14:15') AS INTEGER), \
                CAST(strftime('%m', '2026-07-05 13:14:15') AS INTEGER), \
                CAST(strftime('%d', '2026-07-05 13:14:15') AS INTEGER), \
                CAST(strftime('%H', '2026-07-05 13:14:15') AS INTEGER), \
                CAST(strftime('%M', '2026-07-05 13:14:15') AS INTEGER), \
                CAST(strftime('%w', '2026-07-05 13:14:15') AS INTEGER)",
            [],
            |row| {
                Ok([
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ])
            },
        )
        .expect("sqlite extract query");
    let sqlite_second: i64 = sqlite
        .query_row(
            "SELECT CAST(strftime('%S', '2026-07-05 13:14:15.789') AS INTEGER)",
            [],
            |row| row.get(0),
        )
        .expect("sqlite second query");

    let mysql_rows = mysql_query(
        &mysql,
        "SELECT \
            EXTRACT(YEAR FROM CAST('2026-07-05 13:14:15' AS DATETIME(6))) AS year_part, \
            EXTRACT(MONTH FROM CAST('2026-07-05 13:14:15' AS DATETIME(6))) AS month_part, \
            EXTRACT(DAY FROM CAST('2026-07-05 13:14:15' AS DATETIME(6))) AS day_part, \
            EXTRACT(HOUR FROM CAST('2026-07-05 13:14:15' AS DATETIME(6))) AS hour_part, \
            EXTRACT(MINUTE FROM CAST('2026-07-05 13:14:15' AS DATETIME(6))) AS minute_part, \
            (DAYOFWEEK(CAST('2026-07-05 13:14:15' AS DATETIME(6))) - 1) AS dow_part, \
            EXTRACT(SECOND FROM CAST('2026-07-05 13:14:15.789' AS DATETIME(6))) AS second_part",
    )
    .await;
    let mysql_row = mysql_rows.rows.first().expect("one mysql row");
    let mysql_values = [
        json_i64(mysql_row.get("year_part").expect("year_part")),
        json_i64(mysql_row.get("month_part").expect("month_part")),
        json_i64(mysql_row.get("day_part").expect("day_part")),
        json_i64(mysql_row.get("hour_part").expect("hour_part")),
        json_i64(mysql_row.get("minute_part").expect("minute_part")),
        json_i64(mysql_row.get("dow_part").expect("dow_part")),
    ];
    let mysql_second = json_f64(mysql_row.get("second_part").expect("second_part"));

    let expected = [2026, 7, 5, 13, 14, 0];
    assert_eq!(pg_values, expected, "PG portable extract values");
    assert_eq!(sqlite_values, expected, "SQLite portable extract values");
    assert_eq!(mysql_values, expected, "MySQL portable extract values");

    assert!(
        (pg_second - 15.789).abs() < 0.000_001,
        "PG second preserves fractional seconds: {pg_second}"
    );
    assert_eq!(sqlite_second, 15, "SQLite strftime('%S') is integer seconds");
    assert_eq!(mysql_second, 15.0, "MySQL EXTRACT(SECOND) is integer seconds");
}
