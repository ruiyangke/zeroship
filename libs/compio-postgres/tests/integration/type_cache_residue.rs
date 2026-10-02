use compio_postgres::{Client, Error, QueryOutcome};
use futures_util::StreamExt;
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

async fn create_failing_custom_type_fixture(client: &Client, type_name: &str) {
    client
        .batch_execute(&format!(
            "CREATE TYPE pg_temp.{type_name} AS ENUM ('value'); BEGIN"
        ))
        .await
        .expect("create the custom type and begin its failing transaction");
}

fn failing_custom_type_query(type_name: &str) -> String {
    format!(
        "SELECT NULL::pg_temp.{type_name} \
         FROM generate_series(0, 0) AS g(n) \
         WHERE $1::int4 / n::int4 = 0"
    )
}

#[compio::test]
async fn query_text_params_preserves_the_outer_error_when_type_resolution_fails() {
    const BARRIER: &str = "SELECT 1 /* cpg_query_diag_observer_barrier */";

    let url = test_url();
    let client = connect(&url)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    let mut events = client.query_events();
    let type_name = common::test_object_name("cpg_query_diag_text_enum");
    create_failing_custom_type_fixture(&client, &type_name).await;
    let query = failing_custom_type_query(&type_name);

    let failure = client
        .query_text_params(&query, &["1"])
        .await
        .expect_err("query_text_params execution must fail with division by zero");
    assert_eq!(
        failure.code().map(compio_postgres::error::SqlState::code),
        Some("22012"),
        "query_text_params discarded SQLSTATE 22012 behind catalog SQLSTATE 25P02: {failure}"
    );

    client
        .batch_execute("ROLLBACK")
        .await
        .expect("the outer query error left the transaction recoverable");
    client
        .simple_query(BARRIER)
        .await
        .expect("the outer query error left the session observable");

    let observed = compio::time::timeout(Duration::from_secs(5), async {
        let mut observed = Vec::new();
        loop {
            let event = events
                .next()
                .await
                .expect("query observer closed before the barrier");
            let reached_barrier = event.sql() == BARRIER;
            observed.push(event);
            if reached_barrier {
                break observed;
            }
        }
    })
    .await
    .expect("query observer did not reach the barrier");
    let target: Vec<_> = observed
        .iter()
        .filter(|event| event.sql() == query)
        .collect();
    assert_eq!(
        target.len(),
        1,
        "out-of-band outer error emitted zero or multiple events"
    );
    match target[0].outcome() {
        QueryOutcome::DatabaseError { code } => assert_eq!(
            code.as_ref(),
            Some(&compio_postgres::error::SqlState::DIVISION_BY_ZERO)
        ),
        other => panic!("expected database error observation, got {other:?}"),
    }

    let value: i32 = client
        .query_one("SELECT 1", &[])
        .await
        .expect("the outer query error left the session reusable")
        .get(0);
    assert_eq!(value, 1);
}

#[compio::test]
async fn query_typed_preserves_the_outer_error_when_type_resolution_fails() {
    let url = test_url();
    let client = connect(&url)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    let type_name = common::test_object_name("cpg_query_diag_typed_enum");
    create_failing_custom_type_fixture(&client, &type_name).await;
    let query = failing_custom_type_query(&type_name);

    let failure = client
        .query_typed(&query, &[(&1_i32, compio_postgres::types::Type::INT4)])
        .await
        .expect_err("query_typed execution must fail with division by zero");
    assert_eq!(
        failure.code().map(compio_postgres::error::SqlState::code),
        Some("22012"),
        "query_typed discarded SQLSTATE 22012 behind catalog SQLSTATE 25P02: {failure}"
    );

    client
        .batch_execute("ROLLBACK")
        .await
        .expect("the outer query error left the transaction recoverable");
    let value: i32 = client
        .query_one("SELECT 1", &[])
        .await
        .expect("the outer query error left the session reusable")
        .get(0);
    assert_eq!(value, 1);
}

/// `PostgreSQL`'s recursive containment check follows `typelem` only for true
/// arrays. A custom base type can therefore advertise a composite as its
/// element while using a different subscript handler, and that composite can
/// contain the base type. The catalog is healthy and the values are usable,
/// but this resolver follows raw `typelem` and sees base -> composite -> base.
#[compio::test]
async fn accepted_custom_element_cycle_is_reported() {
    compio::time::timeout(Duration::from_secs(10), async {
        let url = test_url();
        let client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let composite = common::test_object_name("cpg_cycle_composite");
        let base = common::test_object_name("cpg_cycle_base");
        let input = common::test_object_name("cpg_cycle_input");
        let output = common::test_object_name("cpg_cycle_output");

        client
            .batch_execute(&format!(
                "CREATE TYPE pg_temp.{composite} AS (n int4); \
                 CREATE TYPE pg_temp.{base}; \
                 CREATE FUNCTION pg_temp.{input}(cstring) \
                     RETURNS pg_temp.{base} AS 'jsonb_in' \
                     LANGUAGE internal IMMUTABLE STRICT; \
                 CREATE FUNCTION pg_temp.{output}(pg_temp.{base}) \
                     RETURNS cstring AS 'jsonb_out' \
                     LANGUAGE internal IMMUTABLE STRICT; \
                 CREATE TYPE pg_temp.{base} ( \
                     INPUT = pg_temp.{input}, \
                     OUTPUT = pg_temp.{output}, \
                     INTERNALLENGTH = variable, \
                     STORAGE = extended, \
                     ELEMENT = pg_temp.{composite}, \
                     SUBSCRIPT = pg_catalog.jsonb_subscript_handler \
                 ); \
                 ALTER TYPE pg_temp.{composite} \
                     ADD ATTRIBUTE b pg_temp.{base}"
            ))
            .await
            .expect("PostgreSQL rejected its valid custom-element cycle");

        let oid: i64 = client
            .query_one(
                &format!("SELECT 'pg_temp.{base}'::regtype::oid::int8"),
                &[],
            )
            .await
            .expect("read the custom base type OID without resolving its shape")
            .get(0);
        let failure = client
            .prepare(&format!("SELECT NULL::pg_temp.{base}"))
            .await
            .expect_err("the recursive type graph was accepted as acyclic");
        assert_eq!(
            common::error_chain(&failure),
            format!(
                "error parsing response from server: cycle detected resolving postgres type with OID {oid}"
            )
        );

        let value: i32 = client
            .query_one("SELECT 1", &[])
            .await
            .expect("cycle detection left the session reusable")
            .get(0);
        assert_eq!(value, 1);
    })
    .await
    .expect("custom-element cycle test exceeded its watchdog");
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
        let range_name = common::test_object_name("cpg_stale_range");
        let range_ddl = format!("CREATE TYPE pg_temp.{range_name} AS RANGE (subtype = int4)");
        let range_query = format!("SELECT NULL::pg_temp.{range_name}");
        assert_stale_helpers_are_retired(&range, &range_ddl, &range_query, 1).await;

        let enumeration = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let enum_name = common::test_object_name("cpg_stale_enum");
        let enum_ddl = format!("CREATE TYPE pg_temp.{enum_name} AS ENUM ('value')");
        let enum_query = format!("SELECT NULL::pg_temp.{enum_name}");
        assert_stale_helpers_are_retired(&enumeration, &enum_ddl, &enum_query, 2).await;

        let composite = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let composite_name = common::test_object_name("cpg_stale_composite");
        let composite_ddl = format!("CREATE TEMP TABLE {composite_name} (value int4)");
        let composite_query = format!("SELECT NULL::pg_temp.{composite_name}");
        assert_stale_helpers_are_retired(&composite, &composite_ddl, &composite_query, 2).await;
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
