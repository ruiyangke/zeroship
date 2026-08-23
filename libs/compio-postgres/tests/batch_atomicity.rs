//! A multi-statement `batch_execute` is ATOMIC; a sequence of them is not.
//!
//! PostgreSQL wraps a simple query carrying several statements in an IMPLICIT
//! transaction block, so if any statement fails the whole query is rolled back.
//! Send the same statements as separate simple queries and each one commits on
//! its own, so an earlier success survives a later failure.
//!
//! The two differ only in FRAMING -- identical SQL, identical order -- which is
//! exactly why this is worth pinning. A caller reading `batch_execute("A; B")`
//! has no syntactic cue that it behaves unlike `batch_execute("A")` followed by
//! `batch_execute("B")`, and the difference only shows up once something fails.
//! Nothing in the suite covered it.
//!
//! It is also a property this driver could lose without any test noticing: if
//! `batch_execute` were ever changed to split its input and send one statement
//! per query -- a plausible thing to do while chasing a parser or a timeout --
//! the atomicity would silently disappear and callers relying on it would start
//! leaving half-applied work behind.
//!
//! Measured against the live server on 2026-08-23 before being written down:
//! `psql -c "INSERT ...; SELECT 1/0"` leaves 0 rows, while the same two
//! statements as `-c "INSERT ..." -c "SELECT 1/0"` leave 1.

use compio_postgres::{Client, NoTls};

#[allow(dead_code)]
mod common;

fn test_url() -> Option<String> {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .expect("connect to PostgreSQL");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

/// Create a fresh temporary table and return its name.
///
/// Temporary, so nothing survives the session even if an assertion below fails
/// part way through -- the review database is shared with other suites.
async fn probe_table(client: &Client, suffix: &str) -> String {
    let name = common::test_object_name(&format!("cpg_atomic_{suffix}"));
    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {name} (v int)"))
        .await
        .expect("create the probe table");
    name
}

async fn row_count(client: &Client, table: &str) -> i64 {
    client
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .expect("count rows")
        .get(0)
}

/// One `batch_execute` carrying a failing statement rolls the whole thing back.
#[compio::test]
async fn a_failure_inside_one_batch_rolls_back_its_earlier_statements() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;
    let table = probe_table(&client, "one").await;

    client
        .batch_execute(&format!("INSERT INTO {table} VALUES (1); SELECT 1/0"))
        .await
        .expect_err("the batch must fail on the division");

    assert_eq!(
        row_count(&client, &table).await,
        0,
        "the INSERT survived a failure later in the SAME simple query; a \
         multi-statement batch runs in an implicit transaction block and must \
         be all-or-nothing"
    );
}

/// The control, differing in ONE variable: the same statements, split across
/// two `batch_execute` calls, are NOT atomic.
///
/// Without this the test above would also be satisfied by a driver that had
/// somehow failed to run the INSERT at all, or by a server that rolled back
/// everything unconditionally.
#[compio::test]
async fn the_same_statements_sent_separately_are_not_atomic() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;
    let table = probe_table(&client, "two").await;

    client
        .batch_execute(&format!("INSERT INTO {table} VALUES (1)"))
        .await
        .expect("the INSERT alone succeeds");
    client
        .batch_execute("SELECT 1/0")
        .await
        .expect_err("the division fails on its own");

    assert_eq!(
        row_count(&client, &table).await,
        1,
        "a committed INSERT was undone by a LATER, SEPARATE failing query -- \
         separate simple queries each commit on their own"
    );
}

/// An explicit `BEGIN` inside a batch does not change the outcome, but it is
/// worth pinning because it is where a caller's intuition most often breaks:
/// the block is already implicit, so the `COMMIT` is what makes the work
/// durable past the failing statement that follows it.
#[compio::test]
async fn an_explicit_commit_inside_a_batch_makes_earlier_work_durable() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
    let client = connect_client(&url).await;
    let table = probe_table(&client, "commit").await;

    client
        .batch_execute(&format!(
            "BEGIN; INSERT INTO {table} VALUES (1); COMMIT; SELECT 1/0"
        ))
        .await
        .expect_err("the batch still fails on the division");

    assert_eq!(
        row_count(&client, &table).await,
        1,
        "an explicitly COMMITted statement inside a batch must survive a later \
         failure in the same batch"
    );
}
