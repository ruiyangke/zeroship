use compio_postgres::{Client, Error};
use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

fn test_url() -> String {
    common::test_url()
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls()).await?;
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

/// The connection task, rather than the caller consuming `ErrorResponse`, must
/// retire a stale internal helper. Otherwise abandoning the failed lookup
/// leaves the same cached server name for the next operation to hit again.
#[compio::test]
async fn abandoned_stale_typeinfo_lookup_cleans_the_cache_for_the_next_lookup() {
    compio::time::timeout(Duration::from_secs(10), async {
        let url = test_url();
        let client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let type_name = common::test_object_name("cpg_abandoned_stale_range");
        client
            .batch_execute(&format!(
                "CREATE TYPE pg_temp.{type_name} AS RANGE (subtype = int4)"
            ))
            .await
            .expect("create the connection-local range type");
        let query = format!("SELECT NULL::pg_temp.{type_name}");
        let original = client
            .prepare(&query)
            .await
            .expect("prime the internal type-info helper");

        client
            .batch_execute("DEALLOCATE ALL")
            .await
            .expect("remove the helper's server-side statement");
        client.clear_type_cache();

        let mut lookup = Box::pin(client.prepare(&query));
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            matches!(lookup.as_mut().poll(&mut context), Poll::Pending),
            "custom-type prepare completed before its initial request was queued"
        );

        // The first request is Parse + Describe for `query`. Wait for its
        // ReadyForQuery, then consume its possibly split response batches one
        // poll at a time. The poll which starts the cached type-info query
        // increments the connection's in-flight counter synchronously. Dropping
        // immediately on that transition abandons the helper response without
        // relying on scheduler timing or a server-side sleep.
        loop {
            while client.transaction_status().is_none() {
                compio::time::sleep(Duration::from_millis(1)).await;
            }

            assert!(
                matches!(lookup.as_mut().poll(&mut context), Poll::Pending),
                "stale helper error reached the caller before the cancellation edge"
            );
            if client.transaction_status().is_none() {
                break;
            }
            compio::time::sleep(Duration::from_millis(1)).await;
        }
        drop(lookup);

        // Drain the abandoned helper's ErrorResponse + ReadyForQuery before
        // probing the same connection. The original user Statement stays alive
        // so its stale drop-time Close cannot supply an unrelated 26000.
        client
            .simple_query("")
            .await
            .expect("abandoned helper response left the connection misaligned");
        client
            .prepare(&query)
            .await
            .expect("abandoned stale helper was reused by the next lookup");
        drop(original);
    })
    .await
    .expect("abandoned type-info cleanup test exceeded its watchdog");
}
