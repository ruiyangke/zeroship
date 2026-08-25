use compio_postgres::Client;
use compio_postgres::types::Type;
use std::time::Duration;

mod common;

const QUERY_START_TIMEOUT: Duration = Duration::from_secs(5);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(10);

fn test_url() -> String {
    common::test_url()
}

async fn connect() -> Client {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", common::error_chain(&error));
        }
    })
    .detach();
    client
}

async fn assert_same_client_recovers(client: &Client, process_id: i32) {
    let value: i32 = compio::time::timeout(
        RECOVERY_TIMEOUT,
        client.query_one_scalar("SELECT 73::int4", &[]),
    )
    .await
    .expect("the same client stayed blocked after the timed-out query")
    .expect("the same client was poisoned after the timed-out query");

    assert_eq!(value, 73);
    assert_eq!(client.process_id(), process_id);
}

#[compio::test]
async fn text_params_resolves_custom_types_without_response_backpressure_deadlock() {
    let client = connect().await;
    let process_id = client.process_id();
    client
        .batch_execute("CREATE TYPE pg_temp.cpg_query_text_enum AS ENUM ('value')")
        .await
        .unwrap();

    let query = "\
        SELECT 'value'::pg_temp.cpg_query_text_enum, repeat('x', 16384) \
        FROM generate_series(1, 2048)";
    let result =
        compio::time::timeout(QUERY_START_TIMEOUT, client.query_text_params(query, &[])).await;
    let completed = match result {
        Ok(Ok(stream)) => {
            drop(stream);
            true
        }
        Ok(Err(error)) => panic!("query_text_params failed before returning its stream: {error}"),
        Err(_) => false,
    };

    assert_same_client_recovers(&client, process_id).await;
    assert!(
        completed,
        "query_text_params deadlocked while resolving an uncached custom type behind a full response channel"
    );
}

#[compio::test]
async fn query_typed_resolves_custom_types_without_response_backpressure_deadlock() {
    let client = connect().await;
    let process_id = client.process_id();
    client
        .batch_execute("CREATE TYPE pg_temp.cpg_query_typed_enum AS ENUM ('value')")
        .await
        .unwrap();

    let query = "\
        SELECT 'value'::pg_temp.cpg_query_typed_enum, repeat('x', 16384) \
        FROM generate_series(1, 2048)";
    let result = compio::time::timeout(
        QUERY_START_TIMEOUT,
        client.query_typed_raw(query, std::iter::empty::<(&str, Type)>()),
    )
    .await;
    let completed = match result {
        Ok(Ok(stream)) => {
            drop(stream);
            true
        }
        Ok(Err(error)) => panic!("query_typed_raw failed before returning its stream: {error}"),
        Err(_) => false,
    };

    assert_same_client_recovers(&client, process_id).await;
    assert!(
        completed,
        "query_typed_raw deadlocked while resolving an uncached custom type behind a full response channel"
    );
}
