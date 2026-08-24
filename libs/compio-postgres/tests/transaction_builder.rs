//! Server-observed coverage for the public transaction builder.

use compio_postgres::{Client, Error, IsolationLevel, NoTls, Transaction};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);

fn test_url() -> String {
    common::test_url()
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", common::error_chain(&error));
        }
    })
    .detach();
    Ok(client)
}

fn expect_ok<T>(result: Result<T, Error>, context: &str) -> T {
    result.unwrap_or_else(|error| {
        let sqlstate = error.code().map_or("none", |code| code.code());
        panic!(
            "{context} failed with SQLSTATE {sqlstate}: {}",
            common::error_chain(&error)
        )
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ReportedState {
    isolation: String,
    read_only: String,
    deferrable: String,
}

async fn show(transaction: &Transaction<'_>, setting: &str) -> String {
    let query = format!("SHOW {setting}");
    expect_ok(transaction.query_one(&query, &[]).await, &query).get(0)
}

async fn reported_state(transaction: &Transaction<'_>) -> ReportedState {
    ReportedState {
        isolation: show(transaction, "transaction_isolation").await,
        read_only: show(transaction, "transaction_read_only").await,
        deferrable: show(transaction, "transaction_deferrable").await,
    }
}

async fn set_defaults(
    client: &Client,
    isolation: &str,
    read_only: bool,
    deferrable: bool,
) {
    let sql = format!(
        "SET default_transaction_isolation = '{isolation}'; \
         SET default_transaction_read_only = {}; \
         SET default_transaction_deferrable = {}",
        if read_only { "on" } else { "off" },
        if deferrable { "on" } else { "off" },
    );
    expect_ok(client.batch_execute(&sql).await, "set transaction defaults");
}

#[compio::test]
async fn no_options_start_a_usable_transaction_with_server_defaults() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let table = common::test_object_name("cpg_transaction_builder_default");
        expect_ok(
            client
                .batch_execute(&format!("CREATE TEMP TABLE {table} (value int4 NOT NULL)"))
                .await,
            "create default-builder fixture",
        );
        set_defaults(&client, "repeatable read", false, true).await;

        let transaction = expect_ok(
            client.build_transaction().start().await,
            "start transaction without options",
        );
        assert_eq!(
            reported_state(&transaction).await,
            ReportedState {
                isolation: "repeatable read".to_string(),
                read_only: "off".to_string(),
                deferrable: "on".to_string(),
            }
        );
        assert_eq!(
            expect_ok(
                transaction
                    .execute(&format!("INSERT INTO {table} VALUES (41)"), &[])
                    .await,
                "insert through default transaction",
            ),
            1
        );
        expect_ok(transaction.rollback().await, "roll back default transaction");

        let remaining: i64 = expect_ok(
            client
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await,
            "count rows after default transaction rollback",
        )
        .get(0);
        assert_eq!(remaining, 0, "the no-option builder did not open a transaction");
    })
    .await
    .expect("no-option transaction builder test exceeded its 20 second deadline");
}

#[compio::test]
async fn every_isolation_level_reaches_the_server() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        // PostgreSQL reports the nominal `read uncommitted` label even though
        // it implements that level with READ COMMITTED semantics. Keep the
        // server's report here rather than pretending the label is rewritten.
        let cases = [
            (IsolationLevel::ReadUncommitted, "read uncommitted"),
            (IsolationLevel::ReadCommitted, "read committed"),
            (IsolationLevel::RepeatableRead, "repeatable read"),
            (IsolationLevel::Serializable, "serializable"),
        ];

        for (level, expected) in cases {
            let opposite_default = if expected == "serializable" {
                "read committed"
            } else {
                "serializable"
            };
            set_defaults(&client, opposite_default, false, false).await;

            let transaction = expect_ok(
                client
                    .build_transaction()
                    .isolation_level(level)
                    .start()
                    .await,
                "start transaction with isolation level",
            );
            assert_eq!(
                reported_state(&transaction).await,
                ReportedState {
                    isolation: expected.to_string(),
                    read_only: "off".to_string(),
                    deferrable: "off".to_string(),
                },
                "server did not apply {level:?}"
            );
            expect_ok(transaction.rollback().await, "roll back isolation transaction");
        }
    })
    .await
    .expect("isolation-level transaction builder test exceeded its 20 second deadline");
}

#[compio::test]
async fn read_only_and_deferrable_apply_both_boolean_values() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        for requested in [false, true] {
            set_defaults(&client, "read committed", !requested, false).await;
            let transaction = expect_ok(
                client
                    .build_transaction()
                    .read_only(requested)
                    .start()
                    .await,
                "start transaction with read-only mode",
            );
            assert_eq!(
                reported_state(&transaction).await,
                ReportedState {
                    isolation: "read committed".to_string(),
                    read_only: if requested { "on" } else { "off" }.to_string(),
                    deferrable: "off".to_string(),
                }
            );
            expect_ok(transaction.rollback().await, "roll back read-only transaction");
        }

        for requested in [false, true] {
            set_defaults(&client, "read committed", false, !requested).await;
            let transaction = expect_ok(
                client
                    .build_transaction()
                    .deferrable(requested)
                    .start()
                    .await,
                "start transaction with deferrable mode",
            );
            assert_eq!(
                reported_state(&transaction).await,
                ReportedState {
                    isolation: "read committed".to_string(),
                    read_only: "off".to_string(),
                    deferrable: if requested { "on" } else { "off" }.to_string(),
                }
            );
            expect_ok(transaction.rollback().await, "roll back deferrable transaction");
        }
    })
    .await
    .expect("boolean transaction builder test exceeded its 20 second deadline");
}

#[compio::test]
async fn every_multiple_option_shape_reaches_the_server() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        set_defaults(&client, "serializable", false, false).await;
        let transaction = expect_ok(
            client
                .build_transaction()
                .isolation_level(IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()
                .await,
            "start isolation plus read-only transaction",
        );
        assert_eq!(
            reported_state(&transaction).await,
            ReportedState {
                isolation: "repeatable read".to_string(),
                read_only: "on".to_string(),
                deferrable: "off".to_string(),
            }
        );
        expect_ok(transaction.rollback().await, "roll back isolation/read-only transaction");

        set_defaults(&client, "serializable", false, false).await;
        let transaction = expect_ok(
            client
                .build_transaction()
                .isolation_level(IsolationLevel::RepeatableRead)
                .deferrable(true)
                .start()
                .await,
            "start isolation plus deferrable transaction",
        );
        assert_eq!(
            reported_state(&transaction).await,
            ReportedState {
                isolation: "repeatable read".to_string(),
                read_only: "off".to_string(),
                deferrable: "on".to_string(),
            }
        );
        expect_ok(transaction.rollback().await, "roll back isolation/deferrable transaction");

        set_defaults(&client, "read committed", false, false).await;
        let transaction = expect_ok(
            client
                .build_transaction()
                .read_only(true)
                .deferrable(true)
                .start()
                .await,
            "start read-only plus deferrable transaction",
        );
        assert_eq!(
            reported_state(&transaction).await,
            ReportedState {
                isolation: "read committed".to_string(),
                read_only: "on".to_string(),
                deferrable: "on".to_string(),
            }
        );
        expect_ok(transaction.rollback().await, "roll back read-only/deferrable transaction");

        set_defaults(&client, "read committed", false, false).await;
        let transaction = expect_ok(
            client
                .build_transaction()
                .isolation_level(IsolationLevel::Serializable)
                .read_only(true)
                .deferrable(true)
                .start()
                .await,
            "start transaction with every option",
        );
        assert_eq!(
            reported_state(&transaction).await,
            ReportedState {
                isolation: "serializable".to_string(),
                read_only: "on".to_string(),
                deferrable: "on".to_string(),
            }
        );
        expect_ok(transaction.rollback().await, "roll back all-option transaction");
    })
    .await
    .expect("combined transaction builder test exceeded its 20 second deadline");
}
