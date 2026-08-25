//! What a caller is told when a message exceeds this driver's size limit.
//!
//! `buf_stream::MAX_MESSAGE_SIZE` caps a single backend message at 64 MB -
//! deliberately below PostgreSQL's own 1 GB, because a header claiming a
//! gigabyte would otherwise make this driver buffer one. That cap is a design
//! decision and not what these tests are about.
//!
//! What they are about is the DIAGNOSIS. `validate_length` builds a precise
//! error - "message too large: N bytes (max M)" - and the caller used to
//! receive `connection closed` with an empty cause chain, because the
//! connection task died holding the explanation while the waiting request only
//! saw its response channel drop. A limit nobody can discover from the error
//! reads exactly like an unexplained disconnect, and the first thing a user
//! does about an unexplained disconnect is retry it.

#[allow(dead_code)]
mod common;

use common::{suite_tls, test_url};
use compio_postgres::{Client, Error};

/// One megabyte, as the server counts it.
const MB: usize = 1024 * 1024;

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

/// Ask the server for a single text value of `bytes` bytes.
async fn select_value_of(client: &Client, bytes: usize) -> Result<usize, Error> {
    let row = client
        .query_one(&format!("SELECT repeat('x', {bytes})::text"), &[])
        .await?;
    let value: &str = row.get(0);
    Ok(value.len())
}

fn chain_of(error: &Error) -> String {
    let mut rendered = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        rendered.push_str(" | ");
        rendered.push_str(&cause.to_string());
        source = std::error::Error::source(cause);
    }
    rendered
}

/// THE CONTROL. A value comfortably under the cap round-trips, so a failure
/// above it is the cap and not "large values do not work".
#[compio::test]
async fn a_value_under_the_limit_round_trips() {
    let client = connected().await;
    let len = select_value_of(&client, 32 * MB)
        .await
        .expect("32 MB is well under the limit");
    assert_eq!(len, 32 * MB);
}

/// The error a caller actually receives has to NAME the limit. Reporting
/// `connection closed` with no cause is the defect: the driver knows exactly
/// what happened and does not say.
#[compio::test]
async fn a_message_over_the_limit_names_the_limit() {
    let client = connected().await;

    let error = select_value_of(&client, 70 * MB)
        .await
        .expect_err("70 MB exceeds the 64 MB message limit");

    let chain = chain_of(&error);
    assert!(
        chain.contains("message too large"),
        "the caller cannot tell that a size limit was hit; they were told: {chain}"
    );
    assert!(
        chain.contains("max"),
        "the error does not report what the limit IS, so the caller cannot \
         judge whether their value is unreasonable or the cap is: {chain}"
    );
}
