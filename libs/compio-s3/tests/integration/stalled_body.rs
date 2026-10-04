//! A streaming GET whose body stops arriving ends with a timeout.
//!
//! The per-chunk body bound lives in `S3Client::get_stream`. Its adapter has
//! a unit test over a synthetic stream; this suite shows the client applies
//! it to a real HTTP body, against an endpoint that sends the head of an
//! object and then holds the connection silent.

use std::time::Duration;

use compio_s3::{S3Client, S3Config, S3Credentials, S3Error};
use futures::StreamExt;
use zeroship_testkit::s3::StalledObject;

const SENT: usize = 4096;
const BODY_BOUND: Duration = Duration::from_millis(300);
/// Far past `BODY_BOUND`: a read still waiting here was never bounded.
const TEST_BOUND: Duration = Duration::from_secs(20);

#[test]
fn a_stalled_body_ends_in_a_timeout_after_the_bytes_that_arrived() {
    let endpoint = StalledObject::start(SENT, 1024 * 1024);
    let mut config = S3Config::parse_url(&endpoint.url("stall")).expect("endpoint configuration");
    config.timeouts.body = BODY_BOUND;
    let client = S3Client::new(config, S3Credentials::new("stall", "stall-secret", None));

    let (received, ending) = compio::runtime::Runtime::new().unwrap().block_on(async move {
        compio::time::timeout(TEST_BOUND, async {
            let (meta, body) = client.get_stream("object.bin").await.expect("object head");
            assert_eq!(meta.len, 1024 * 1024);
            futures::pin_mut!(body);
            let mut received = 0;
            loop {
                match body.next().await {
                    Some(Ok(frame)) => received += frame.len(),
                    Some(Err(error)) => break (received, error),
                    None => panic!("the body ended after {received} bytes instead of failing"),
                }
            }
        })
        .await
        .expect("the stalled body was still being read past the body bound")
    });

    assert!(endpoint.has_stalled(), "the endpoint never reached its stall");
    assert_eq!(received, SENT, "every byte sent before the stall is delivered");
    match ending {
        S3Error::Retryable { status: None, ref detail } if detail.contains("body read timeout") => {}
        other => panic!("expected the body-read timeout, got {other:?}"),
    }
}
