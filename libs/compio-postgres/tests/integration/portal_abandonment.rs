//! A portal abandoned with rows still queued must not wedge the session.
//!
//! `query_portal` with a row limit leaves the server holding an open portal
//! and the client holding a partial answer. The failure this guards is not an
//! error but SILENT CORRUPTION: undrained `DataRow`s left in the buffer would
//! be read by the next request as its own result, so the next query returns
//! someone else's rows and nothing anywhere reports a problem.
//!
//! Every case therefore ends by asking an unrelated question with a
//! recognisable answer. Checking that the abandoned operation "failed cleanly"
//! would not catch this; only the NEXT operation can.

#[allow(unused_imports)]
use crate::common;
use common::{suite_tls, test_url};
use compio_postgres::Client;

/// A value no query in this file could produce by accident, so reading it back
/// proves the answer came from the question that was asked.
const SENTINEL: i32 = 424_242;

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

/// Ask an unrelated question and insist on its own answer.
async fn assert_session_answers_its_own_question(client: &Client, after: &str) {
    let answer: i32 = client
        .query_one_scalar("SELECT $1::int4", &[&SENTINEL])
        .await
        .unwrap_or_else(|error| panic!("the session was unusable after {after}: {error}"));
    assert_eq!(
        answer, SENTINEL,
        "after {after} the session answered with someone else's rows"
    );
}

#[compio::test]
async fn a_portal_abandoned_before_a_commit_leaves_the_session_clean() {
    let mut client = connected().await;

    let transaction = client.transaction().await.expect("begin");
    let portal = transaction
        .bind("SELECT g FROM generate_series(1, 100) g", &[])
        .await
        .expect("bind a portal over 100 rows");
    let page = transaction
        .query_portal(&portal, 3)
        .await
        .expect("fetch the first three");
    assert_eq!(page.len(), 3, "the row limit was not honoured");
    assert_eq!(page[0].get::<_, i32>(0), 1, "the portal did not start at 1");

    // 97 rows are still queued on the server.
    drop(portal);
    transaction
        .commit()
        .await
        .expect("an abandoned portal must not block the commit");

    assert_session_answers_its_own_question(&client, "abandoning a portal and committing").await;
}

#[compio::test]
async fn a_portal_abandoned_before_a_rollback_leaves_the_session_clean() {
    let mut client = connected().await;

    let transaction = client.transaction().await.expect("begin");
    let portal = transaction
        .bind("SELECT g FROM generate_series(1, 100) g", &[])
        .await
        .expect("bind a portal over 100 rows");
    let page = transaction
        .query_portal(&portal, 2)
        .await
        .expect("fetch the first two");
    assert_eq!(page.len(), 2, "the row limit was not honoured");

    drop(portal);
    // Dropping the transaction rolls it back, with the portal still open.
    drop(transaction);

    assert_session_answers_its_own_question(&client, "abandoning a portal and rolling back").await;
}

/// THE CONTROL. A portal read to exhaustion must yield every row and stop, so
/// the abandonment cases above are about ABANDONING and not about portals
/// being broken generally. It also pins the end condition: the last page is
/// empty rather than short-then-hanging.
#[compio::test]
async fn a_portal_drained_in_pages_yields_every_row_once() {
    let mut client = connected().await;

    let transaction = client.transaction().await.expect("begin");
    let portal = transaction
        .bind("SELECT g FROM generate_series(1, 10) g", &[])
        .await
        .expect("bind a portal over 10 rows");

    let mut seen = Vec::new();
    // Bounded so a portal that never reports exhaustion fails here rather than
    // hanging the suite.
    for _ in 0..10 {
        let page = transaction
            .query_portal(&portal, 4)
            .await
            .expect("fetch a page");
        if page.is_empty() {
            break;
        }
        seen.extend(page.iter().map(|row| row.get::<_, i32>(0)));
    }

    assert_eq!(
        seen,
        (1..=10).collect::<Vec<i32>>(),
        "a paged drain did not yield each row exactly once, in order"
    );

    transaction.commit().await.expect("commit");
    assert_session_answers_its_own_question(&client, "draining a portal").await;
}
