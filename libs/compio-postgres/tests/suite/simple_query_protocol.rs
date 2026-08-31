//! PostgreSQL's allowed SIMPLE `Query` reply sequences.

#[allow(unused_imports)]
use crate::common;
use compio_postgres::error::SqlState;
use compio_postgres::types::Type;
use compio_postgres::{Client, SimpleQueryFormat, SimpleQueryMessage};
use futures_util::StreamExt;

async fn connected() -> Client {
    let url = common::test_url();
    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

#[derive(Debug, Eq, PartialEq)]
enum Seen {
    Description(Vec<String>),
    Row(Vec<Option<String>>),
    Complete(u64),
}

fn flatten(message: &SimpleQueryMessage) -> Seen {
    match message {
        SimpleQueryMessage::RowDescription(columns) => Seen::Description(
            columns
                .iter()
                .map(|column| column.name().to_string())
                .collect(),
        ),
        SimpleQueryMessage::Row(row) => Seen::Row(
            (0..row.len())
                .map(|index| {
                    row.try_get(index)
                        .expect("the characterization query returns text")
                        .map(str::to_string)
                })
                .collect(),
        ),
        SimpleQueryMessage::CommandComplete(rows) => Seen::Complete(*rows),
        _ => panic!("unexpected simple-query message: {message:?}"),
    }
}

/// One `Query` can mix statements with and without rows. Each statement's
/// replies remain in wire order and each RowDescription replaces the previous
/// statement's layout without ending the request.
#[compio::test]
async fn mixed_statements_preserve_every_reply_in_wire_order() {
    let client = connected().await;
    let table = common::test_object_name("cpg_simple_mixed");
    let messages = client
        .simple_query(&format!(
            "SELECT 11::int4 AS first; \
             CREATE TEMPORARY TABLE {table} (id int); \
             INSERT INTO {table} VALUES (2), (1); \
             SELECT id FROM {table} ORDER BY id; \
             DROP TABLE {table}"
        ))
        .await
        .expect("the mixed simple query succeeds");

    assert_eq!(
        messages.iter().map(flatten).collect::<Vec<_>>(),
        vec![
            Seen::Description(vec!["first".to_string()]),
            Seen::Row(vec![Some("11".to_string())]),
            Seen::Complete(1),
            Seen::Complete(0),
            Seen::Complete(2),
            Seen::Description(vec!["id".to_string()]),
            Seen::Row(vec![Some("1".to_string())]),
            Seen::Row(vec![Some("2".to_string())]),
            Seen::Complete(2),
            Seen::Complete(0),
        ]
    );
}

/// An execution error can follow replies from both an earlier statement and
/// the failing statement itself. The raw stream preserves that prefix, reports
/// the server error once, and emits nothing for the abandoned third statement.
#[compio::test]
async fn raw_stream_preserves_the_prefix_before_a_middle_statement_error() {
    let client = connected().await;
    let stream = client
        .simple_query_raw(
            "SELECT 11::int4 AS first; \
             SELECT 10 / n AS partial FROM generate_series(1, 0, -1) AS series(n); \
             SELECT 33::int4 AS skipped",
        )
        .await
        .expect("enqueue the three-statement query");
    let mut stream = std::pin::pin!(stream);
    let mut prefix = Vec::new();

    let error = loop {
        match stream.next().await {
            Some(Ok(message)) => prefix.push(flatten(&message)),
            Some(Err(error)) => break error,
            None => panic!("the stream ended before the division-by-zero error"),
        }
    };

    assert_eq!(
        prefix,
        vec![
            Seen::Description(vec!["first".to_string()]),
            Seen::Row(vec![Some("11".to_string())]),
            Seen::Complete(1),
            Seen::Description(vec!["partial".to_string()]),
            Seen::Row(vec![Some("10".to_string())]),
        ],
        "the caller did not receive exactly the replies sent before the error"
    );
    assert_eq!(error.code(), Some(&SqlState::DIVISION_BY_ZERO));
    assert!(
        stream.next().await.is_none(),
        "the failed statement completed or the abandoned third statement ran"
    );
}

/// Simple-query values are text except for a FETCH from a BINARY cursor.
/// Binary int4 42 is valid UTF-8 (`00 00 00 2a`), so blindly decoding every
/// DataRow as text silently returns four characters instead of reporting that
/// this field uses PostgreSQL's typed binary representation.
#[compio::test]
async fn binary_cursor_fetch_never_exposes_binary_values_as_text() {
    let client = connected().await;
    let messages = client
        .simple_query(
            "BEGIN; \
             DECLARE cpg_simple_binary BINARY CURSOR FOR \
                 SELECT 42::int4 AS answer, NULL::int4 AS null_value; \
             FETCH ALL FROM cpg_simple_binary; \
             COMMIT",
        )
        .await
        .expect("the binary cursor query itself succeeds");

    let columns = messages
        .iter()
        .find_map(|message| match message {
            SimpleQueryMessage::RowDescription(columns) if columns[0].name() == "answer" => {
                Some(columns)
            }
            _ => None,
        })
        .expect("FETCH returned no RowDescription");
    assert_eq!(columns[0].format(), SimpleQueryFormat::Binary);
    assert_eq!(columns[0].type_oid(), Type::INT4.oid());
    assert_eq!(columns[1].format(), SimpleQueryFormat::Binary);
    assert_eq!(columns[1].type_oid(), Type::INT4.oid());

    let row = messages
        .iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .expect("FETCH returned no DataRow");
    let decoded = row.try_get("answer");
    assert!(
        decoded.is_err(),
        "binary int4 bytes were exposed as text: {decoded:?}"
    );
    let error = decoded.expect_err("the binary value was exposed as text");
    let cause = common::error_chain(&error);
    assert!(
        cause.contains("column is in binary format; use SimpleQueryRow::raw_value"),
        "the text-access error did not direct the caller to the raw bytes: {cause}"
    );
    assert_eq!(
        row.raw_value("answer").expect("answer is a column"),
        Some(&42i32.to_be_bytes()[..]),
        "the legal binary value was not preserved for the caller"
    );
    assert_eq!(row.try_get("null_value").unwrap(), None);
    assert_eq!(row.raw_value("null_value").unwrap(), None);
    assert_eq!(
        row.raw_value("absent").unwrap_err().to_string(),
        "invalid column `absent`"
    );
    assert_eq!(
        row.raw_value(2).unwrap_err().to_string(),
        "invalid column `2`"
    );
}
