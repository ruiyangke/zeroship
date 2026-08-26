//! Every concurrent request on one connection gets ITS OWN answer.
//!
//! `run_multiplexed` splits the socket and reads while it writes, so responses
//! are matched to waiters by order rather than by any identifier on the wire.
//! That makes routing the core invariant of the whole loop, and a slip in it
//! is SILENT: query A simply returns B's rows. Nothing errors, no connection
//! dies, and every existing test that checks "did this query succeed" passes.
//!
//! So each request here asks a question only it could have asked, and the
//! assertion is on the MAPPING rather than on success. The suite pairs two
//! futures elsewhere (cancellation, backend death); what it did not do is
//! drive many distinguishable requests at once and check who got what.

#[allow(unused_imports)]
use crate::common;
use common::{suite_tls, test_url};
use compio_postgres::Client;

/// Enough concurrency that an off-by-one in routing cannot stay hidden behind
/// a single in-flight request.
const CONCURRENT: i32 = 32;

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

/// Ask `sentinel` back as its own answer, so the reply identifies its request.
async fn echo(client: &Client, sentinel: i32) -> (i32, Result<i32, compio_postgres::Error>) {
    (
        sentinel,
        client
            .query_one_scalar::<i32, _>("SELECT $1::int4", &[&sentinel])
            .await,
    )
}

/// Report every reply that went to the wrong request.
fn misroutings(results: &[(i32, Result<i32, compio_postgres::Error>)]) -> Vec<String> {
    results
        .iter()
        .filter_map(|(sent, got)| match got {
            Ok(answer) if answer == sent => None,
            Ok(answer) => Some(format!("request {sent} received request {answer}'s answer")),
            Err(error) => Some(format!("request {sent} failed: {error}")),
        })
        .collect()
}

#[compio::test]
async fn concurrent_requests_each_receive_their_own_answer() {
    let client = connected().await;

    let results =
        futures_util::future::join_all((0..CONCURRENT).map(|sentinel| echo(&client, sentinel)))
            .await;

    assert_eq!(results.len(), CONCURRENT as usize);
    let wrong = misroutings(&results);
    assert!(
        wrong.is_empty(),
        "responses were routed to the wrong requests:\n  {}",
        wrong.join("\n  ")
    );
}

/// A slow request in the middle changes the ORDER replies arrive in relative
/// to the order requests were sent. If routing tracked arrival rather than
/// registration, this is where it would show.
#[compio::test]
async fn a_slow_request_does_not_misroute_the_fast_ones_beside_it() {
    let client = connected().await;

    let slow = async {
        client
            .batch_execute("SELECT pg_sleep(0.4)")
            .await
            .expect("the slow query completes")
    };
    let fast = futures_util::future::join_all((100..110).map(|sentinel| echo(&client, sentinel)));

    let (_, results) = futures_util::future::join(slow, fast).await;

    let wrong = misroutings(&results);
    assert!(
        wrong.is_empty(),
        "a slow request beside fast ones misrouted them:\n  {}",
        wrong.join("\n  ")
    );
}

/// An ERROR has to reach the request that caused it and no other. A failure
/// delivered to the wrong waiter fails a query that was fine and silently
/// succeeds one that was not.
#[compio::test]
async fn a_failing_request_takes_its_error_and_nobody_elses_result() {
    let client = connected().await;

    let failing = client.query_one_scalar::<i32, _>("SELECT 1 / 0", &[]);
    let succeeding =
        futures_util::future::join_all((200..205).map(|sentinel| echo(&client, sentinel)));

    let (failed, results) = futures_util::future::join(failing, succeeding).await;

    let error = failed.expect_err("division by zero cannot succeed");
    assert_eq!(
        error.code().map(|code| code.code().to_owned()).as_deref(),
        Some("22012"),
        "the failing request did not receive its own error: {error}"
    );

    let wrong = misroutings(&results);
    assert!(
        wrong.is_empty(),
        "a failing request disturbed its neighbours:\n  {}",
        wrong.join("\n  ")
    );

    // And the connection is still usable, so the error was delivered rather
    // than the session being torn down to get rid of it.
    let after: i32 = client
        .query_one_scalar("SELECT 777::int4", &[])
        .await
        .expect("the connection survives a failed concurrent request");
    assert_eq!(after, 777);
}

/// THE CONTROL. `misroutings` has to be able to SEE a misrouting, or the three
/// tests above pass on any behaviour at all.
#[test]
fn the_misrouting_check_can_fail() {
    let swapped = vec![(1, Ok(2)), (2, Ok(1))];
    assert_eq!(
        misroutings(&swapped).len(),
        2,
        "two swapped answers were not reported as misrouted"
    );
    let correct = vec![(1, Ok(1)), (2, Ok(2))];
    assert!(misroutings(&correct).is_empty());
}
