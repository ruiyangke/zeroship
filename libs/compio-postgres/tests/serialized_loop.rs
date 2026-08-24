//! Behaviour on the SERIALIZED connection loop, reached without TLS.
//!
//! `Connection::run` picks its loop by whether the transport splits into owned
//! halves. A plain socket splits and takes the multiplexed loop; a TLS stream
//! cannot - rustls keeps shared session state - so every TLS connection takes
//! the serialized one. The loops are not equivalent, so behaviour proven over
//! plaintext is not thereby proven over TLS, and until now the only way into
//! the serialized loop from a test was to stand up a TLS server. That is why
//! the two diverged with nothing going red (task #49).
//!
//! `test_utils::connect_serialized` makes that loop reachable over plaintext.
//! These tests are the parity net for the refactor that will delete the
//! serialized loop: they must keep passing when TLS and plaintext share one.

use compio_postgres::test_utils::connect_serialized;
use compio_postgres::{Client, Config};

#[allow(dead_code)]
mod common;

async fn serialized_client_with_probe() -> (Client, std::rc::Rc<std::cell::Cell<bool>>) {
    let url = common::test_url();
    let config: Config = url.parse().expect("test DSN did not parse");
    let (client, connection, split_refused) = connect_serialized(&config)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!(
                "serialized connection error: {}",
                common::error_chain(&error)
            );
        }
    })
    .detach();
    (client, split_refused)
}

async fn serialized_client() -> Client {
    serialized_client_with_probe().await.0
}

/// The harness must actually be on the serialized loop, or everything below
/// silently measures the multiplexed one and proves nothing about TLS.
///
/// The evidence is the SPLIT REFUSAL itself, recorded by the socket when
/// `Connection::run` asks it to split. Nothing else here discriminates: a
/// stable backend pid, a working query and a committed transaction are all
/// equally true on the multiplexed loop, so a test asserting those would pass
/// whichever loop it had reached.
#[compio::test]
async fn the_harness_is_really_on_the_serialized_loop() {
    let (client, split_refused) = serialized_client_with_probe().await;
    let first: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("first query")
        .get(0);
    let second: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("second query")
        .get(0);
    assert_eq!(first, second, "the harness reconnected between queries");
    assert!(
        split_refused.get(),
        "Connection::run never asked this socket to split, so these tests are \
         not on the serialized loop and prove nothing about TLS"
    );
}

/// Ordinary queries, parameters and row decoding work on this loop.
#[compio::test]
async fn queries_and_parameters_work_on_the_serialized_loop() {
    let client = serialized_client().await;
    let row = client
        .query_one("SELECT $1::int8 + 1, $2::text", &[&41i64, &"hello"])
        .await
        .expect("parameterised query");
    assert_eq!(row.get::<_, i64>(0), 42);
    assert_eq!(row.get::<_, &str>(1), "hello");
}

/// A server error must surface with its SQLSTATE and leave the session usable.
#[compio::test]
async fn an_error_leaves_the_serialized_session_usable() {
    let client = serialized_client().await;
    let Err(error) = client.query_one("SELECT 1/0", &[]).await else {
        panic!("division by zero must fail");
    };
    assert_eq!(
        error.code().map(|code| code.code()),
        Some("22012"),
        "the error lost its SQLSTATE: {}",
        common::error_chain(&error)
    );
    let row = client
        .query_one("SELECT 1::int4", &[])
        .await
        .expect("the session must survive a server error");
    assert_eq!(row.get::<_, i32>(0), 1);
}

/// Transactions commit and roll back on this loop.
#[compio::test]
async fn transactions_work_on_the_serialized_loop() {
    let mut client = serialized_client().await;
    let table = common::test_object_name("cpg serialized tx");
    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {table}; CREATE TABLE {table}(id int)"
        ))
        .await
        .expect("fixture");

    let transaction = client.transaction().await.expect("begin");
    transaction
        .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
        .await
        .expect("insert");
    transaction.rollback().await.expect("rollback");

    let row = client
        .query_one(&format!("SELECT count(*)::int8 FROM {table}"), &[])
        .await
        .expect("count");
    assert_eq!(row.get::<_, i64>(0), 0, "the rollback did not take");

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
}
