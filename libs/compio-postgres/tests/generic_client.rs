//! Rollback-sensitive coverage for every method on the public `GenericClient` trait.

use compio_postgres::types::{ToSql, Type};
use compio_postgres::{
    Client, Error, GenericClient, NoTls, Row, SimpleQueryMessage, Transaction,
};
use futures_util::TryStreamExt;
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

fn assert_one_row(rows: &[Row], marker: i32, method: &str) {
    assert_eq!(rows.len(), 1, "GenericClient::{method} returned the wrong row count");
    assert_eq!(
        rows[0].get::<_, i32>(0),
        marker,
        "GenericClient::{method} returned the wrong marker"
    );
}

async fn collect_raw(stream: compio_postgres::RowStream, marker: i32, method: &str) {
    let mut stream = Box::pin(stream);
    let mut rows = Vec::new();
    while let Some(row) = expect_ok(stream.as_mut().try_next().await, method) {
        rows.push(row);
    }
    assert_one_row(&rows, marker, method);
    assert_eq!(
        stream.as_ref().get_ref().rows_affected(),
        Some(1),
        "GenericClient::{method} did not finish the INSERT stream"
    );
}

/// Exercise every `GenericClient` method taking `&self`.
///
/// Keeping this generic is essential: a direct `transaction.execute(...)`
/// call resolves to the inherent method and would never touch the trait impl.
async fn exercise_shared_methods<C>(client: &C, table: &str, offset: i32) -> Vec<i32>
where
    C: GenericClient + Sync,
{
    let insert = |marker| format!("INSERT INTO {table} (marker) VALUES (${marker})");
    let returning = |marker| {
        format!("INSERT INTO {table} (marker) VALUES (${marker}) RETURNING marker")
    };
    let mut inserted = Vec::with_capacity(16);

    let marker = offset + 1;
    let sql = insert(1);
    assert_eq!(
        expect_ok(
            <C as GenericClient>::execute(client, &sql, &[&marker]).await,
            "GenericClient::execute",
        ),
        1
    );
    inserted.push(marker);

    let marker = offset + 2;
    let sql = insert(1);
    assert_eq!(
        expect_ok(
            <C as GenericClient>::execute_raw(client, &sql, std::iter::once(&marker)).await,
            "GenericClient::execute_raw",
        ),
        1
    );
    inserted.push(marker);

    let marker = offset + 3;
    let sql = insert(1);
    let params: [(&(dyn ToSql + Sync), Type); 1] = [(&marker, Type::INT4)];
    assert_eq!(
        expect_ok(
            <C as GenericClient>::execute_typed(client, &sql, &params).await,
            "GenericClient::execute_typed",
        ),
        1
    );
    inserted.push(marker);

    let marker = offset + 4;
    let sql = returning(1);
    let rows = expect_ok(
        <C as GenericClient>::query(client, &sql, &[&marker]).await,
        "GenericClient::query",
    );
    assert_one_row(&rows, marker, "query");
    inserted.push(marker);

    let marker = offset + 5;
    let sql = returning(1);
    let row = expect_ok(
        <C as GenericClient>::query_one(client, &sql, &[&marker]).await,
        "GenericClient::query_one",
    );
    assert_eq!(row.get::<_, i32>(0), marker);
    inserted.push(marker);

    let marker = offset + 6;
    let sql = returning(1);
    let row = expect_ok(
        <C as GenericClient>::query_opt(client, &sql, &[&marker]).await,
        "GenericClient::query_opt",
    )
    .expect("GenericClient::query_opt returned None for INSERT RETURNING");
    assert_eq!(row.get::<_, i32>(0), marker);
    inserted.push(marker);

    let marker = offset + 7;
    let sql = returning(1);
    let stream = expect_ok(
        <C as GenericClient>::query_raw(client, &sql, std::iter::once(&marker)).await,
        "GenericClient::query_raw",
    );
    collect_raw(stream, marker, "query_raw").await;
    inserted.push(marker);

    let marker = offset + 8;
    let sql = returning(1);
    let params: [(&(dyn ToSql + Sync), Type); 1] = [(&marker, Type::INT4)];
    let rows = expect_ok(
        <C as GenericClient>::query_typed(client, &sql, &params).await,
        "GenericClient::query_typed",
    );
    assert_one_row(&rows, marker, "query_typed");
    inserted.push(marker);

    let marker = offset + 9;
    let sql = returning(1);
    let params: [(&(dyn ToSql + Sync), Type); 1] = [(&marker, Type::INT4)];
    let row = expect_ok(
        <C as GenericClient>::query_typed_one(client, &sql, &params).await,
        "GenericClient::query_typed_one",
    );
    assert_eq!(row.get::<_, i32>(0), marker);
    inserted.push(marker);

    let marker = offset + 10;
    let sql = returning(1);
    let params: [(&(dyn ToSql + Sync), Type); 1] = [(&marker, Type::INT4)];
    let row = expect_ok(
        <C as GenericClient>::query_typed_opt(client, &sql, &params).await,
        "GenericClient::query_typed_opt",
    )
    .expect("GenericClient::query_typed_opt returned None for INSERT RETURNING");
    assert_eq!(row.get::<_, i32>(0), marker);
    inserted.push(marker);

    let marker = offset + 11;
    let sql = returning(1);
    let stream = expect_ok(
        <C as GenericClient>::query_typed_raw(
            client,
            &sql,
            std::iter::once((&marker, Type::INT4)),
        )
        .await,
        "GenericClient::query_typed_raw",
    );
    collect_raw(stream, marker, "query_typed_raw").await;
    inserted.push(marker);

    let marker = offset + 12;
    let sql = insert(1);
    let statement = expect_ok(
        <C as GenericClient>::prepare(client, &sql).await,
        "GenericClient::prepare",
    );
    assert_eq!(
        expect_ok(
            <C as GenericClient>::execute(client, &statement, &[&marker]).await,
            "execute GenericClient::prepare result",
        ),
        1
    );
    inserted.push(marker);

    let marker = offset + 13;
    let sql = insert(1);
    let statement = expect_ok(
        <C as GenericClient>::prepare_typed(client, &sql, &[Type::INT4]).await,
        "GenericClient::prepare_typed",
    );
    assert_eq!(
        expect_ok(
            <C as GenericClient>::execute(client, &statement, &[&marker]).await,
            "execute GenericClient::prepare_typed result",
        ),
        1
    );
    inserted.push(marker);

    let marker = offset + 14;
    let sql = format!("INSERT INTO {table} (marker) VALUES ({marker})");
    expect_ok(
        <C as GenericClient>::batch_execute(client, &sql).await,
        "GenericClient::batch_execute",
    );
    inserted.push(marker);

    let marker = offset + 15;
    let sql = format!("INSERT INTO {table} (marker) VALUES ({marker}) RETURNING marker");
    let messages = expect_ok(
        <C as GenericClient>::simple_query(client, &sql).await,
        "GenericClient::simple_query",
    );
    let returned = messages.iter().find_map(|message| match message {
        SimpleQueryMessage::Row(row) => row.get(0),
        _ => None,
    });
    assert_eq!(returned, Some(marker.to_string().as_str()));
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, SimpleQueryMessage::CommandComplete(1))),
        "GenericClient::simple_query did not report one inserted row"
    );
    inserted.push(marker);

    let marker = offset + 16;
    let sql = insert(1);
    let underlying = <C as GenericClient>::client(client);
    assert_eq!(
        expect_ok(
            underlying.execute(&sql, &[&marker]).await,
            "work through GenericClient::client",
        ),
        1
    );
    inserted.push(marker);

    inserted
}

async fn stored_markers(client: &Client, table: &str) -> Vec<i32> {
    expect_ok(
        client
            .query(&format!("SELECT marker FROM {table} ORDER BY marker"), &[])
            .await,
        "read GenericClient markers",
    )
    .into_iter()
    .map(|row| row.get(0))
    .collect()
}

async fn generic_transaction<'a, C>(client: &'a mut C) -> Result<Transaction<'a>, Error>
where
    C: GenericClient,
{
    <C as GenericClient>::transaction(client).await
}

#[compio::test]
async fn shared_methods_for_client_and_transaction_remain_inside_the_transaction() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let table = common::test_object_name("cpg_generic_client_shared");
        expect_ok(
            client
                .batch_execute(&format!(
                    "CREATE TEMP TABLE {table} (marker int4 PRIMARY KEY)"
                ))
                .await,
            "create GenericClient shared-method fixture",
        );

        let transaction = expect_ok(client.transaction().await, "start outer transaction");
        let mut expected = exercise_shared_methods(&transaction, &table, 0).await;
        expected.extend(exercise_shared_methods(transaction.client(), &table, 100).await);
        expected.sort_unstable();
        assert_eq!(stored_markers(transaction.client(), &table).await, expected);

        expect_ok(transaction.rollback().await, "roll back GenericClient operations");
        assert!(
            stored_markers(&client, &table).await.is_empty(),
            "a GenericClient method escaped the transaction rollback"
        );
    })
    .await
    .expect("GenericClient shared-method test exceeded its 20 second deadline");
}

#[compio::test]
async fn transaction_method_preserves_top_level_and_nested_rollback_scopes() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let table = common::test_object_name("cpg_generic_client_transaction");
        expect_ok(
            client
                .batch_execute(&format!(
                    "CREATE TEMP TABLE {table} (marker int4 PRIMARY KEY)"
                ))
                .await,
            "create GenericClient transaction-method fixture",
        );

        let transaction = expect_ok(
            generic_transaction(&mut client).await,
            "GenericClient::transaction for Client",
        );
        assert_eq!(
            expect_ok(
                <Transaction<'_> as GenericClient>::execute(
                    &transaction,
                    &format!("INSERT INTO {table} VALUES ($1)"),
                    &[&1_i32],
                )
                .await,
                "insert through top-level generic transaction",
            ),
            1
        );
        expect_ok(transaction.rollback().await, "roll back top-level generic transaction");
        assert!(stored_markers(&client, &table).await.is_empty());

        let mut outer = expect_ok(client.transaction().await, "start outer transaction");
        let nested = expect_ok(
            generic_transaction(&mut outer).await,
            "GenericClient::transaction for Transaction",
        );
        assert_eq!(
            expect_ok(
                <Transaction<'_> as GenericClient>::execute(
                    &nested,
                    &format!("INSERT INTO {table} VALUES ($1)"),
                    &[&2_i32],
                )
                .await,
                "insert through nested generic transaction",
            ),
            1
        );
        expect_ok(nested.commit().await, "commit nested generic transaction");
        assert_eq!(stored_markers(outer.client(), &table).await, [2]);
        expect_ok(outer.rollback().await, "roll back outer transaction");
        assert!(
            stored_markers(&client, &table).await.is_empty(),
            "GenericClient::transaction escaped its enclosing transaction"
        );
    })
    .await
    .expect("GenericClient transaction-method test exceeded its 20 second deadline");
}
