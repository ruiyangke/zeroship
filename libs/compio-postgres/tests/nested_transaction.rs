//! Nested transactions and savepoints, checked against the SERVER.
//!
//! A defect here is silent and expensive: a nested transaction whose rollback
//! does not actually undo its writes leaves committed rows the caller believes
//! they discarded, and nothing errors. So every case reads the surviving rows
//! back rather than trusting what `commit`/`rollback` returned - the return
//! value is the driver's opinion, and the table is the fact.
//!
//! `transaction.rs` was among the least-covered non-TLS files when this was
//! written, and nothing exercised the client-side savepoint API: the only
//! other tests mentioning savepoints are about decoding them from the
//! replication stream, which is a different question entirely.

#[allow(dead_code)]
mod common;
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
    let table = test_object_name("nested_tx");
    client
        .batch_execute(&format!("CREATE TABLE {table} (id int primary key)"))
        .await
        .expect("create the fixture table");
    table
}

/// What actually survived, asked of the server.
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
async fn an_inner_rollback_keeps_the_outer_writes_and_discards_its_own() {
    let mut client = connected().await;
    let table = fixture(&client).await;

    {
        let mut outer = client.transaction().await.expect("begin the outer");
        outer
            .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .expect("the outer write");
        {
            let inner = outer.transaction().await.expect("begin the inner");
            inner
                .execute(&format!("INSERT INTO {table} VALUES (2)"), &[])
                .await
                .expect("the inner write");
            inner.rollback().await.expect("roll the inner back");
        }
        outer.commit().await.expect("commit the outer");
    }

    assert_eq!(
        surviving_ids(&client, &table).await,
        vec![1],
        "an inner rollback did not discard exactly its own write"
    );
    drop_fixture(&client, &table).await;
}

#[compio::test]
async fn an_inner_commit_contributes_to_the_outer() {
    let mut client = connected().await;
    let table = fixture(&client).await;

    {
        let mut outer = client.transaction().await.expect("begin the outer");
        outer
            .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .expect("the outer write");
        {
            let inner = outer.transaction().await.expect("begin the inner");
            inner
                .execute(&format!("INSERT INTO {table} VALUES (2)"), &[])
                .await
                .expect("the inner write");
            inner.commit().await.expect("commit the inner");
        }
        outer.commit().await.expect("commit the outer");
    }

    assert_eq!(
        surviving_ids(&client, &table).await,
        vec![1, 2],
        "a committed inner transaction did not contribute to the outer"
    );
    drop_fixture(&client, &table).await;
}

/// THE ONE THAT MATTERS. A committed inner transaction must NOT escape an
/// outer rollback - a savepoint release is not a durable commit. If it did,
/// rolling back would leave rows behind and report success.
#[compio::test]
async fn an_outer_rollback_discards_a_committed_inner() {
    let mut client = connected().await;
    let table = fixture(&client).await;

    {
        let mut outer = client.transaction().await.expect("begin the outer");
        outer
            .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .expect("the outer write");
        {
            let inner = outer.transaction().await.expect("begin the inner");
            inner
                .execute(&format!("INSERT INTO {table} VALUES (2)"), &[])
                .await
                .expect("the inner write");
            inner.commit().await.expect("commit the inner");
        }
        outer.rollback().await.expect("roll the outer back");
    }

    assert_eq!(
        surviving_ids(&client, &table).await,
        Vec::<i32>::new(),
        "a committed inner transaction survived the outer rollback"
    );
    drop_fixture(&client, &table).await;
}

/// Dropping an inner transaction without calling anything must roll it back,
/// the same way dropping an outer one does. Leaking the savepoint instead
/// would silently keep the writes.
#[compio::test]
async fn dropping_an_inner_transaction_rolls_it_back() {
    let mut client = connected().await;
    let table = fixture(&client).await;

    {
        let mut outer = client.transaction().await.expect("begin the outer");
        outer
            .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .expect("the outer write");
        {
            let inner = outer.transaction().await.expect("begin the inner");
            inner
                .execute(&format!("INSERT INTO {table} VALUES (2)"), &[])
                .await
                .expect("the inner write");
            // No commit, no rollback: just dropped.
        }
        outer.commit().await.expect("commit the outer");
    }

    assert_eq!(
        surviving_ids(&client, &table).await,
        vec![1],
        "a dropped inner transaction kept its write"
    );
    drop_fixture(&client, &table).await;
}

/// The same through the NAMED savepoint API, which is a separate entry point
/// to the same machinery and could diverge from the anonymous one.
#[compio::test]
async fn a_named_savepoint_rolls_back_only_its_own_writes() {
    let mut client = connected().await;
    let table = fixture(&client).await;

    {
        let mut outer = client.transaction().await.expect("begin the outer");
        outer
            .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .expect("the outer write");
        {
            let savepoint = outer.savepoint("nested_tx_probe").await.expect("savepoint");
            savepoint
                .execute(&format!("INSERT INTO {table} VALUES (2)"), &[])
                .await
                .expect("the savepoint write");
            savepoint.rollback().await.expect("roll the savepoint back");
        }
        outer.commit().await.expect("commit the outer");
    }

    assert_eq!(
        surviving_ids(&client, &table).await,
        vec![1],
        "a named savepoint rollback did not discard exactly its own write"
    );
    drop_fixture(&client, &table).await;
}
