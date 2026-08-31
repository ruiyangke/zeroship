//! Regression coverage for the two Debug/non-Debug parameter encoding copies.
//!
//! This target owns its process because `log::set_logger` is process-global.
//! Its logger enables only this driver's query Debug target, making the branch
//! choice and the emitted parameter rendering independently observable.

use compio_postgres::{Client, NoTls};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::sync::{Mutex, Once};

#[allow(dead_code, clippy::doc_markdown)]
#[path = "common/env.rs"]
mod test_env;

const QUERY_LOG_TARGET: &str = "compio_postgres::query";

struct QueryLogger;

impl Log for QueryLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() == Level::Debug && metadata.target() == QUERY_LOG_TARGET
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            QUERY_LOGS
                .lock()
                .expect("lock captured query logs")
                .push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

static QUERY_LOGGER: QueryLogger = QueryLogger;
static INSTALL_QUERY_LOGGER: Once = Once::new();
static QUERY_LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn install_query_logger() {
    INSTALL_QUERY_LOGGER.call_once(|| {
        log::set_logger(&QUERY_LOGGER).expect("install the query Debug test logger");
        log::set_max_level(LevelFilter::Debug);
    });
}

// Compio futures are deliberately thread-local; this helper is driven by the
// `#[compio::test]` runtime on the same thread.
#[allow(clippy::future_not_send)]
async fn client() -> Client {
    let url = test_env::get(test_env::TestEnvKey::PgTestUrl)
        .expect("PG_TEST_URL must name the PostgreSQL test server");
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .expect("connect to the PostgreSQL test server");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("query Debug test connection error: {error}");
        }
    })
    .detach();
    client
}

fn assert_one_parameter_log(value: i32) {
    let prefix = "executing statement ";
    let suffix = format!(" with parameters: [{value}]");
    let logs = QUERY_LOGS.lock().expect("lock captured query logs").clone();
    let matches = logs
        .iter()
        .filter(|message| {
            let Some(statement_name) = message
                .strip_prefix(prefix)
                .and_then(|message| message.strip_suffix(&suffix))
            else {
                return false;
            };

            statement_name.strip_prefix('s').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
        .count();

    assert_eq!(
        matches, 1,
        "expected one {QUERY_LOG_TARGET} Debug record ending in {suffix:?}; captured {logs:?}",
    );
}

#[compio::test]
async fn query_logs_its_parameters_when_debug_is_enabled() {
    const VALUE: i32 = 1_101_101;

    install_query_logger();
    let client = client().await;
    let statement = client
        .prepare("SELECT $1::int4 /* cpg_query_debug_log */")
        .await
        .expect("prepare query logging statement");
    let rows = client
        .query(&statement, &[&VALUE])
        .await
        .expect("run parameterized query");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i32>(0), VALUE);
    assert_one_parameter_log(VALUE);
}

#[compio::test]
async fn execute_logs_its_parameters_when_debug_is_enabled() {
    const VALUE: i32 = 2_202_202;

    install_query_logger();
    let client = client().await;
    let statement = client
        .prepare("SELECT $1::int4 /* cpg_execute_debug_log */")
        .await
        .expect("prepare execute logging statement");
    let rows = client
        .execute(&statement, &[&VALUE])
        .await
        .expect("execute parameterized statement");

    assert_eq!(rows, 1);
    assert_one_parameter_log(VALUE);
}
