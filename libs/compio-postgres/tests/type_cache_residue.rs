use compio_postgres::{Client, Error, NoTls};
use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    Ok(client)
}

async fn assert_stale_helpers_are_retired(
    client: &Client,
    ddl: &str,
    query: &str,
    stale_helper_count: usize,
) {
    client
        .batch_execute(ddl)
        .await
        .expect("create a connection-local custom type");
    let original = client
        .prepare(query)
        .await
        .expect("prime the type-info statement cache");

    client
        .batch_execute("DEALLOCATE ALL")
        .await
        .expect("remove every server-side prepared statement");
    client.clear_type_cache();

    for index in 0..stale_helper_count {
        match client.prepare(query).await {
            Ok(_) => panic!("stale helper {index} survived its failed lookup"),
            Err(error) => {
                assert_eq!(
                    error.code(),
                    Some(&compio_postgres::error::SqlState::INVALID_SQL_STATEMENT_NAME),
                    "stale helper {index} did not fail with PostgreSQL's 26000"
                );
            }
        }
    }

    client
        .prepare(query)
        .await
        .expect("the failed lookup did not evict its stale type-info statement");

    // Keep the original client-side handle alive through the assertion so its
    // drop-time Close cannot add an unrelated 26000 to the protocol.
    drop(original);
}

#[compio::test]
async fn stale_typeinfo_statement_failure_cleans_the_cache_for_the_next_lookup() {
    compio::time::timeout(Duration::from_secs(10), async {
        let url = test_url();
        let range = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        assert_stale_helpers_are_retired(
            &range,
            "CREATE TYPE pg_temp.cpg_stale_range AS RANGE (subtype = int4)",
            "SELECT NULL::pg_temp.cpg_stale_range",
            1,
        )
        .await;

        let enumeration = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        assert_stale_helpers_are_retired(
            &enumeration,
            "CREATE TYPE pg_temp.cpg_stale_enum AS ENUM ('value')",
            "SELECT NULL::pg_temp.cpg_stale_enum",
            2,
        )
        .await;

        let composite = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        assert_stale_helpers_are_retired(
            &composite,
            "CREATE TEMP TABLE cpg_stale_composite (value int4)",
            "SELECT NULL::pg_temp.cpg_stale_composite",
            2,
        )
        .await;
    })
    .await
    .expect("type-info cache cleanup test exceeded its watchdog");
}
