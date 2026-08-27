//! COPY owns a distinct protocol mode. Ordinary requests must be rejected
//! while its caller-visible handle is active, and a pool lease must not return
//! an active COPY session to another borrower.

use bytes::Bytes;
use compio_postgres::{Client, Error};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);
const REFUSAL_TIMEOUT: Duration = Duration::from_secs(1);

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

fn assert_mode_refusal(error: Error, direction: &str) {
    assert_eq!(
        error.to_string(),
        format!("cannot queue commands during COPY {direction}"),
        "the refusal did not name the active COPY direction"
    );
}

#[compio::test]
async fn copy_in_refuses_queries_until_the_sink_finishes() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let client = connect_client().await;
        let table = common::test_object_name("copy_in_interleaving");
        client
            .batch_execute(&format!("CREATE TEMPORARY TABLE {table} (v int4)"))
            .await
            .expect("create COPY IN fixture");
        let sentinel = client
            .prepare("SELECT 41::int4")
            .await
            .expect("prepare the ordinary query before COPY mode");

        let sink = client
            .copy_in::<_, Bytes>(&format!("COPY {table} FROM STDIN"))
            .await
            .expect("start COPY IN");
        let mut sink = Box::pin(sink);

        let error = compio::time::timeout(REFUSAL_TIMEOUT, client.query_one(&sentinel, &[]))
            .await
            .expect("the ordinary query queued behind COPY IN instead of being refused")
            .expect_err("the ordinary query was accepted during COPY IN");
        assert_mode_refusal(error, "IN");

        sink.as_mut()
            .send(Bytes::from_static(b"7\n"))
            .await
            .expect("send one COPY row");
        assert_eq!(sink.as_mut().finish().await.expect("finish COPY IN"), 1);

        let value: i32 = client
            .query_one(&sentinel, &[])
            .await
            .expect("a finished COPY IN still blocked ordinary queries")
            .get(0);
        assert_eq!(value, 41);
    })
    .await
    .expect("COPY IN interleaving test exceeded its watchdog");
}

#[compio::test]
async fn copy_out_refuses_queries_until_the_stream_finishes() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let client = connect_client().await;
        let sentinel = client
            .prepare("SELECT 42::int4")
            .await
            .expect("prepare the ordinary query before COPY mode");
        let stream = client
            .copy_out(
                "COPY (\
                    SELECT g, pg_sleep(0.002) \
                    FROM generate_series(1, 200) AS g\
                 ) TO STDOUT",
            )
            .await
            .expect("start COPY OUT");
        let mut stream = Box::pin(stream);

        let error = compio::time::timeout(REFUSAL_TIMEOUT, client.query_one(&sentinel, &[]))
            .await
            .expect("the ordinary query queued behind COPY OUT instead of being refused")
            .expect_err("the ordinary query was accepted during COPY OUT");
        assert_mode_refusal(error, "OUT");

        let mut rows = 0;
        while let Some(chunk) = stream.as_mut().next().await {
            rows += chunk
                .expect("read COPY OUT data")
                .iter()
                .filter(|byte| **byte == b'\n')
                .count();
        }
        assert_eq!(rows, 200, "COPY OUT did not deliver every row");

        let value: i32 = client
            .query_one(&sentinel, &[])
            .await
            .expect("a finished COPY OUT still blocked ordinary queries")
            .get(0);
        assert_eq!(value, 42);
    })
    .await
    .expect("COPY OUT interleaving test exceeded its watchdog");
}
