//! COPY statements sent through APIs that cannot carry their data are refused
//! by name, with the supported API in the diagnostic.

use bytes::Bytes;
use compio_postgres::{Client, Error};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);
const COPY_OUT_REFUSAL: &str = "COPY TO STDOUT is not supported by this API; use Client::copy_out";

async fn connect_client() -> Client {
    let url = common::test_url();
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

fn assert_named_copy_out_refusal(api: &str, error: Error) {
    assert_eq!(
        error.to_string(),
        COPY_OUT_REFUSAL,
        "{api} did not name the unsupported COPY direction"
    );
    assert!(
        error.code().is_none(),
        "{api} reported a server error instead of a local refusal: {}",
        common::error_chain(&error)
    );
}

#[compio::test]
async fn unsupported_copy_out_is_refused_by_name_at_every_public_entry_point() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let client = connect_client().await;
        let table = common::test_object_name("copy_out_refusal");
        client
            .batch_execute(&format!(
                "CREATE TEMPORARY TABLE {table} (v int4); \
                 INSERT INTO {table} VALUES (1), (2)"
            ))
            .await
            .expect("create COPY refusal fixture");
        let copy = format!("COPY {table} TO STDOUT");

        let error = client
            .batch_execute(&copy)
            .await
            .expect_err("batch_execute accepted COPY TO STDOUT");
        assert_named_copy_out_refusal("batch_execute", error);

        let error = client
            .simple_query(&copy)
            .await
            .expect_err("simple_query accepted COPY TO STDOUT");
        assert_named_copy_out_refusal("simple_query", error);

        let error = client
            .execute(&copy, &[])
            .await
            .expect_err("execute accepted COPY TO STDOUT");
        assert_named_copy_out_refusal("execute", error);

        let error = client
            .query(&copy, &[])
            .await
            .expect_err("query accepted COPY TO STDOUT");
        assert_named_copy_out_refusal("query", error);

        let error = client
            .execute_text_params(&copy, &[])
            .await
            .expect_err("execute_text_params accepted COPY TO STDOUT");
        assert_named_copy_out_refusal("execute_text_params", error);

        let error = client
            .query_text_params(&copy, &[])
            .await
            .expect_err("query_text_params accepted COPY TO STDOUT");
        assert_named_copy_out_refusal("query_text_params", error);

        let error = client
            .execute_typed(&copy, &[])
            .await
            .expect_err("execute_typed accepted COPY TO STDOUT");
        assert_named_copy_out_refusal("execute_typed", error);

        let error = client
            .query_typed(&copy, &[])
            .await
            .expect_err("query_typed accepted COPY TO STDOUT");
        assert_named_copy_out_refusal("query_typed", error);

        let error = match client.copy_in::<_, Bytes>(&copy).await {
            Ok(_) => panic!("copy_in accepted COPY TO STDOUT"),
            Err(error) => error,
        };
        assert_named_copy_out_refusal("copy_in", error);

        let value: i32 = client
            .query_one_scalar("SELECT 42::int4", &[])
            .await
            .expect("a named COPY refusal left the session unusable");
        assert_eq!(value, 42);
    })
    .await
    .expect("COPY refusal test exceeded its watchdog");
}
