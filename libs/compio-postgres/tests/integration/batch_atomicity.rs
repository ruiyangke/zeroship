//! What survives when a simple-query batch fails part way through.
//!
//! `batch_execute` sends several statements in ONE message, and PostgreSQL
//! runs an unadorned batch inside an implicit transaction. So a failure at
//! statement three discards statements one and two as well - and a caller who
//! believed otherwise, saw the error, and retried would double-apply them.
//!
//! The contrast with the explicit-`COMMIT` case is the point: identical
//! machinery, opposite outcome, decided entirely by what the SQL says. Both
//! are read back from the server, because the driver's return value cannot
//! distinguish them.
//!
//! `simple_query.rs` was among the lowest-covered non-TLS files when this was
//! written.

#[allow(unused_imports)]
use crate::common;
use common::{suite_tls, test_object_name, test_url};
use compio_postgres::Client;

async fn connected() -> Client {
    let (client, connection) = compio_postgres::connect(&test_url(), suite_tls())
        .await
        .expect("connect to the test server");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

async fn fixture(client: &Client) -> String {
    let table = test_object_name("batch_atomic");
    client
        .batch_execute(&format!("CREATE TABLE {table} (id int primary key)"))
        .await
        .expect("create the fixture table");
    table
}

async fn surviving_ids(client: &Client, table: &str) -> Vec<i32> {
    client
        .query(&format!("SELECT id FROM {table} ORDER BY id"), &[])
        .await
        .expect("read the rows back")
        .iter()
        .map(|row| row.get::<_, i32>(0))
        .collect()
}

async fn drop_fixture(client: &Client, table: &str) {
    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
}

#[compio::test]
async fn a_failure_mid_batch_discards_the_statements_before_it() {
    let client = connected().await;
    let table = fixture(&client).await;

    let error = client
        .batch_execute(&format!(
            "INSERT INTO {table} VALUES (1); \
             INSERT INTO {table} VALUES (2); \
             INSERT INTO {table} VALUES ('notanint'); \
             INSERT INTO {table} VALUES (4)"
        ))
        .await
        .expect_err("a non-integer cannot be inserted into an int column");

    assert_eq!(
        error.code().map(|code| code.code().to_owned()).as_deref(),
        Some("22P02"),
        "the batch did not report the failing statement's own error: {error}"
    );
    assert_eq!(
        surviving_ids(&client, &table).await,
        Vec::<i32>::new(),
        "the two statements before the failure were committed; a caller who \
         retries this batch will apply them twice"
    );

    drop_fixture(&client, &table).await;
}

/// The same batch shape with an explicit `COMMIT` before the failure keeps the
/// committed work. Without this case the test above would also pass for a
/// driver that discarded everything unconditionally.
#[compio::test]
async fn work_committed_earlier_in_the_batch_survives_a_later_failure() {
    let client = connected().await;
    let table = fixture(&client).await;

    let error = client
        .batch_execute(&format!(
            "BEGIN; \
             INSERT INTO {table} VALUES (1); \
             COMMIT; \
             INSERT INTO {table} VALUES ('notanint')"
        ))
        .await
        .expect_err("the trailing statement is still malformed");

    assert_eq!(
        error.code().map(|code| code.code().to_owned()).as_deref(),
        Some("22P02"),
        "expected the malformed statement's error: {error}"
    );
    assert_eq!(
        surviving_ids(&client, &table).await,
        vec![1],
        "an explicit COMMIT inside the batch did not make its work durable"
    );

    drop_fixture(&client, &table).await;
}

/// THE CONTROL. A batch with nothing wrong applies every statement, so the
/// two cases above are about FAILURE and not about batches being broken.
#[compio::test]
async fn a_valid_batch_applies_every_statement() {
    let client = connected().await;
    let table = fixture(&client).await;

    client
        .batch_execute(&format!(
            "INSERT INTO {table} VALUES (1); INSERT INTO {table} VALUES (2)"
        ))
        .await
        .expect("a batch with nothing wrong");

    assert_eq!(surviving_ids(&client, &table).await, vec![1, 2]);

    // And the session is usable, so a batch does not leave state behind.
    let answer: i32 = client
        .query_one_scalar("SELECT 909::int4", &[])
        .await
        .expect("the connection works after a batch");
    assert_eq!(answer, 909);

    drop_fixture(&client, &table).await;
}

/// The control differing in ONE variable: the same statements split across TWO
/// `batch_execute` calls are NOT atomic, because each simple query gets its own
/// implicit transaction.
///
/// This is the contrast the whole file exists for. The two forms carry
/// identical SQL in identical order and differ only in FRAMING, so a caller
/// reading `batch_execute("A; B")` has no syntactic cue that it behaves
/// unlike `batch_execute("A")` followed by `batch_execute("B")` - and the
/// difference only appears once something fails.
///
/// It also keeps the first test honest: without this, that test would be
/// satisfied by a driver that never ran the INSERT at all, or by a server
/// rolling everything back unconditionally.
#[compio::test]
async fn the_same_statements_sent_separately_are_not_atomic() {
    let client = connected().await;
    let table = fixture(&client).await;

    client
        .batch_execute(&format!("INSERT INTO {table} VALUES (1)"))
        .await
        .expect("the INSERT alone succeeds");
    client
        .batch_execute("SELECT 1/0")
        .await
        .expect_err("the division fails on its own");

    assert_eq!(
        surviving_ids(&client, &table).await,
        vec![1],
        "a committed INSERT was undone by a LATER, SEPARATE failing query; \
         separate simple queries each commit on their own"
    );

    drop_fixture(&client, &table).await;
}
