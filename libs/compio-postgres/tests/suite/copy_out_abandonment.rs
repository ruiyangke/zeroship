//! A `COPY ... TO STDOUT` stream dropped before exhaustion must not wedge the
//! session.
//!
//! The server is mid-send when the client stops reading, so `CopyData` frames
//! are still arriving for a stream nobody owns. If they are not consumed, or
//! the copy is not properly terminated, the next request reads them as its own
//! answer - the same silent corruption the portal cases guard, reached by a
//! different route.
//!
//! So every case ends by asking an unrelated question with a recognisable
//! answer. "The drop did not panic" would not catch this.

#[allow(unused_imports)]
use crate::common;
use common::{suite_tls, test_url};
use compio_postgres::Client;
use futures_util::StreamExt;

/// Big enough that the server cannot have finished sending before the stream
/// is dropped - roughly 2.5 MB, far past one socket buffer.
const WIDE_QUERY: &str =
    "COPY (SELECT g, repeat('x', 500) FROM generate_series(1, 5000) g) TO STDOUT";

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

async fn assert_answers_its_own_question(client: &Client, sentinel: i32, after: &str) {
    let answer: i32 = client
        .query_one_scalar("SELECT $1::int4", &[&sentinel])
        .await
        .unwrap_or_else(|error| panic!("the session was unusable after {after}: {error}"));
    assert_eq!(
        answer, sentinel,
        "after {after} the session answered with COPY data instead of its own result"
    );
}

#[compio::test]
async fn a_copy_out_dropped_part_way_leaves_the_session_clean() {
    let client = connected().await;

    {
        let stream = client
            .copy_out(WIDE_QUERY)
            .await
            .expect("start the copy out");
        futures_util::pin_mut!(stream);
        let mut chunks = 0;
        while let Some(chunk) = stream.next().await {
            chunk.expect("a chunk arrives");
            chunks += 1;
            if chunks == 3 {
                break;
            }
        }
        assert_eq!(chunks, 3, "the stream ended before it could be abandoned");
    }

    assert_answers_its_own_question(&client, 313_131, "abandoning a COPY OUT part way").await;
}

/// The worst case: the stream is dropped having read NOTHING, so every byte
/// the server sends is unwanted.
#[compio::test]
async fn a_copy_out_dropped_before_reading_anything_leaves_the_session_clean() {
    let client = connected().await;

    drop(
        client
            .copy_out(WIDE_QUERY)
            .await
            .expect("start the copy out"),
    );

    assert_answers_its_own_question(&client, 414_141, "abandoning a COPY OUT immediately").await;
}

/// THE CONTROL. Drained to exhaustion, the stream yields the rows it promised
/// and the session is fine - so the two cases above are about ABANDONMENT and
/// not about COPY OUT being broken.
#[compio::test]
async fn a_drained_copy_out_yields_its_rows() {
    let client = connected().await;

    let mut lines = 0;
    {
        let stream = client
            .copy_out("COPY (SELECT g FROM generate_series(1, 100) g) TO STDOUT")
            .await
            .expect("start the copy out");
        futures_util::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            lines += chunk
                .expect("a chunk arrives")
                .iter()
                .filter(|byte| **byte == b'\n')
                .count();
        }
    }
    assert_eq!(lines, 100, "a drained COPY OUT did not deliver every row");

    assert_answers_its_own_question(&client, 515_151, "draining a COPY OUT").await;
}

/// `ErrorResponse` is the terminal backend message for a failed COPY OUT.
/// The following `ReadyForQuery` belongs to the extended-protocol Sync and
/// must be drained internally, not exposed as a second stream error.
#[compio::test]
async fn copy_out_error_response_terminates_the_stream() {
    let client = connected().await;

    let stream = client
        .copy_out("COPY (SELECT 10 / (3 - g) FROM generate_series(1, 3) g) TO STDOUT")
        .await
        .expect("start the failing copy out");
    futures_util::pin_mut!(stream);

    let server_error = loop {
        match stream.next().await {
            Some(Ok(_)) => {}
            Some(Err(error)) => break error,
            None => panic!("COPY OUT ended without reporting its server error"),
        }
    };
    assert_eq!(
        server_error.code(),
        Some(&compio_postgres::error::SqlState::DIVISION_BY_ZERO),
        "COPY OUT reported the wrong server error: {server_error}"
    );

    match stream.next().await {
        None => {}
        Some(Ok(bytes)) => panic!(
            "COPY OUT yielded {} bytes after its terminal ErrorResponse",
            bytes.len()
        ),
        Some(Err(error)) => {
            panic!("COPY OUT yielded another item after its terminal ErrorResponse: {error}")
        }
    }

    assert_answers_its_own_question(&client, 616_161, "a failed COPY OUT").await;
}
