//! What happens when PostgreSQL kills the backend out from under a live
//! operation.
//!
//! Termination is already covered twice in this suite -- a backend that
//! terminates ITSELF during a `simple_query` (`tests/integration.rs`), and one
//! terminated BETWEEN pool checkouts so the pool's alive-validation path runs
//! (`tests/pool_transaction_isolation.rs`). Neither reaches a backend that dies
//! while an operation is mid-flight, and that is where this driver's bugs have
//! actually lived: the COPY producer teardown, the paused read obligation, the
//! split-reader accounting. Those states carry the most machinery and unwind
//! the least often.
//!
//! A server-side kill is also the one trigger a scripted peer cannot reproduce
//! honestly. The hostile-peer suite ends a connection by closing a socket on
//! cue; this arrives as a real RST against a real in-flight request, with the
//! server's own ErrorResponse racing it.
//!
//! THE LEAK CHECK IS THE ASSERTION THAT MATTERS, not the error. "The call
//! returned an error" is satisfied by a driver that errors AND strands the
//! socket, which is the exact shape the bugs in this area had. Each test
//! therefore pins three outcomes apart: a hang (the watchdog), a false success
//! (the error assertion), and a leak (`live_connections` back at baseline).
//!
//! WHAT THESE DO NOT COVER: a backend that dies without sending anything (a
//! `SIGKILL` of the postmaster child, or the network dropping), which arrives
//! as a bare RST with no ErrorResponse racing it. `pg_terminate_backend` is a
//! polite kill and the server does try to say so first.

use bytes::Bytes;
use compio_postgres::{Client, NoTls};
use futures_util::{SinkExt, TryStreamExt};
use std::time::Duration;

#[allow(dead_code)]
mod common;

/// Long enough that a loaded machine cannot trip it, short enough that a real
/// hang fails this test rather than the whole run. Deliberately generous: see
/// the load table on
/// `read_timeout::copy_input_time_is_not_charged_as_server_read_silence` for
/// what a tight wall-clock budget costs against a live server.
const WATCHDOG: Duration = Duration::from_secs(20);

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

/// Kill `pid` from a second session, because the first one is busy.
async fn terminate(killer: &Client, pid: i32) {
    killer
        .execute("SELECT pg_terminate_backend($1)", &[&pid])
        .await
        .expect("terminate the victim backend");
}

/// A backend killed while a COPY is streaming must fail the COPY and give the
/// socket back.
#[compio::test]
async fn a_backend_killed_mid_copy_fails_the_copy_and_releases_the_connection() {
    let url = test_url();
    let baseline = compio_postgres::live_connections();

    let (victim, victim_connection) = match compio_postgres::connect(&url, NoTls).await {
        Ok(pair) => pair,
        Err(error) => common::postgres_unreachable(&url, &error),
    };
    let victim_driver = compio::runtime::spawn(async move { victim_connection.run().await });
    let (killer, killer_connection) = match compio_postgres::connect(&url, NoTls).await {
        Ok(pair) => pair,
        Err(error) => common::postgres_unreachable(&url, &error),
    };
    let killer_driver = compio::runtime::spawn(async move { killer_connection.run().await });

    let pid: i32 = victim
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("read the victim's backend pid")
        .get(0);

    victim
        .batch_execute("CREATE TEMPORARY TABLE cpg_kill_copy (n int)")
        .await
        .expect("create the COPY target");
    let sink = victim
        .copy_in::<_, Bytes>("COPY cpg_kill_copy (n) FROM STDIN")
        .await
        .expect("enter COPY input mode");
    let mut sink = std::pin::pin!(sink);
    sink.as_mut()
        .send(Bytes::from_static(b"1\n"))
        .await
        .expect("the first COPY row precedes the kill");

    terminate(&killer, pid).await;

    // `send` may well succeed after the kill -- it writes into a local buffer
    // and the RST has not necessarily arrived -- so the outcome under test is
    // the PAIR, not either half. What must not happen is the COPY reporting
    // rows committed to a backend that no longer exists.
    let outcome = compio::time::timeout(WATCHDOG, async {
        let _ = sink.as_mut().send(Bytes::from_static(b"2\n")).await;
        sink.as_mut().finish().await
    })
    .await
    .expect("the COPY hung after its backend was terminated");
    assert!(
        outcome.is_err(),
        "the COPY reported success against a terminated backend: {outcome:?}"
    );

    drop(victim);
    let _ = victim_driver.await;
    drop(killer);
    let _ = killer_driver.await;
    assert_eq!(
        compio_postgres::live_connections(),
        baseline,
        "a backend killed mid-COPY stranded its socket"
    );
}

/// A backend killed while rows are still streaming must fail the stream and
/// give the socket back.
#[compio::test]
async fn a_backend_killed_mid_row_stream_fails_the_stream_and_releases_the_connection() {
    let url = test_url();
    let baseline = compio_postgres::live_connections();

    let (victim, victim_connection) = match compio_postgres::connect(&url, NoTls).await {
        Ok(pair) => pair,
        Err(error) => common::postgres_unreachable(&url, &error),
    };
    let victim_driver = compio::runtime::spawn(async move { victim_connection.run().await });
    let (killer, killer_connection) = match compio_postgres::connect(&url, NoTls).await {
        Ok(pair) => pair,
        Err(error) => common::postgres_unreachable(&url, &error),
    };
    let killer_driver = compio::runtime::spawn(async move { killer_connection.run().await });

    let pid: i32 = victim
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("read the victim's backend pid")
        .get(0);

    // BOTH HALVES OF THIS QUERY ARE LOAD-BEARING, and each replaces a version
    // that made the test pass while proving nothing.
    //
    // `pg_sleep` keeps the server mid-production, so the kill lands during
    // execution rather than after it. It must be in the SELECT LIST: the first
    // version used `WHERE pg_sleep(0.02) IS NULL`, and `void IS NULL` is FALSE,
    // so the predicate filtered out every row. The stream yielded nothing, a
    // second poll of the exhausted stream returned an error, and the assertion
    // below held whether or not the backend was ever killed.
    //
    // `repeat('x', 100000)` is what makes the rows actually STREAM. PostgreSQL
    // buffers its output and, with small rows, flushes nothing until the query
    // completes -- measured: time-to-first-row was 11us for a query taking
    // 4020ms, i.e. the whole result set landed at once and the kill hit an
    // already-finished backend, delivering 200 of 200 rows. At 100 KB per row
    // the buffer fills repeatedly and the first row arrives in ~20ms, so the
    // remaining rows are genuinely still in flight.
    //
    // Only the no-kill control caught either of these.
    let stream = victim
        .query_raw(
            "SELECT repeat('x', 100000), pg_sleep(0.02) FROM generate_series(1, 100) g",
            std::iter::empty::<i32>(),
        )
        .await
        .expect("start the row stream");
    let mut stream = std::pin::pin!(stream);

    let first = stream
        .try_next()
        .await
        .expect("the first row precedes the kill");
    assert!(
        first.is_some(),
        "the query yielded no rows at all, so nothing below is about termination"
    );

    terminate(&killer, pid).await;

    let mut delivered = 1usize;
    let outcome = compio::time::timeout(WATCHDOG, async {
        loop {
            match stream.try_next().await {
                Ok(Some(_)) => {
                    delivered += 1;
                    continue;
                }
                Ok(None) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .expect("the row stream hung after its backend was terminated");
    assert!(
        outcome.is_err(),
        "the row stream ended cleanly against a terminated backend after \
         {delivered} of 100 rows: a truncated result set reported as success"
    );

    drop(victim);
    let _ = victim_driver.await;
    drop(killer);
    let _ = killer_driver.await;
    assert_eq!(
        compio_postgres::live_connections(),
        baseline,
        "a backend killed mid-stream stranded its socket"
    );
}
