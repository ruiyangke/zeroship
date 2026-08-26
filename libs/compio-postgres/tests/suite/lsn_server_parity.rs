//! `parse_lsn` / `format_lsn` agree with POSTGRES, not merely with each other.
//!
//! `replication.rs` already round-trips them in a unit test - but that test
//! feeds our formatter's output to our parser, so it holds for any pair of
//! functions that are consistently wrong. Nothing checked the rendering
//! against the system that defines it.
//!
//! It matters because these two sit on the replication feedback path: a
//! `StandbyStatusUpdate` carries the LSN the client has received, flushed and
//! applied, and the server trims WAL against it. Render it wrong and the
//! consequence is not an error - it is the server discarding WAL a replica
//! still needs, or retaining it forever.
//!
//! The cases below are the ones where a formatter usually goes wrong: zero,
//! a half-word boundary, the maximum, and a value whose low half has leading
//! zeros - PostgreSQL normalises `0/0000000F` to `0/F`, so a formatter that
//! zero-padded would round-trip through itself perfectly and still disagree
//! with every LSN the server ever prints.

#[allow(unused_imports)]
use crate::common;
use common::{suite_tls, test_url};
use compio_postgres::Client;
use compio_postgres::replication::{format_lsn, parse_lsn};

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

/// Written as the server would be asked for them; the server's own rendering
/// is what the test compares against, so these are inputs, not expectations.
const CASES: &[&str] = &[
    "0/0",
    "0/1",
    "0/FFFFFFFF",
    "1/0",
    "0/16B3750",
    "FFFFFFFF/FFFFFFFF",
    // Leading zeros in the low half: the server prints this as `0/F`.
    "0/0000000F",
    "AB/CD",
];

#[compio::test]
async fn our_lsn_text_is_the_servers_lsn_text() {
    let client = connected().await;

    let mut disagreements = Vec::new();
    for case in CASES {
        // Let the SERVER render it. Casting through `pg_lsn` normalises the
        // input, so what comes back is exactly the spelling PostgreSQL uses.
        // `$1::text` first: writing `$1::pg_lsn` makes the server infer the
        // PARAMETER as `pg_lsn`, which a `&str` cannot serialize as. The cast
        // chain still normalises through `pg_lsn`, which is the point.
        let rendered: String = client
            .query_one_scalar("SELECT $1::text::pg_lsn::text", &[case])
            .await
            .unwrap_or_else(|error| panic!("the server could not render {case}: {error}"));

        let Some(parsed) = parse_lsn(&rendered) else {
            disagreements.push(format!(
                "{rendered}: our parser rejected the server's own text"
            ));
            continue;
        };
        let ours = format_lsn(parsed);
        if ours != rendered {
            disagreements.push(format!("{rendered}: we render it as {ours}"));
        }
    }

    assert!(
        disagreements.is_empty(),
        "our LSN text differs from the server's:\n  {}",
        disagreements.join("\n  ")
    );
}

/// A live WAL position, which is the shape that actually travels on the
/// replication feedback path. The constants above are chosen boundaries; this
/// is whatever the server happens to be at.
#[compio::test]
async fn a_live_wal_position_round_trips() {
    let client = connected().await;

    let live: String = client
        .query_one_scalar("SELECT pg_current_wal_lsn()::text", &[])
        .await
        .expect("read the current WAL position");

    let parsed = parse_lsn(&live).expect("our parser accepts a live WAL position");
    assert_eq!(
        format_lsn(parsed),
        live,
        "a live WAL position did not survive our parse and format"
    );
}

/// THE CONTROL. If `format_lsn` zero-padded - the most likely way to be wrong
/// while still self-consistent - the first test would catch it. Prove that
/// comparison can fail, so its passing means something.
#[test]
fn a_padded_rendering_would_be_caught() {
    let value = parse_lsn("0/0000000F").expect("padded input parses");
    assert_eq!(value, 0xF);
    assert_eq!(format_lsn(value), "0/F", "we do not pad");
    assert_ne!(
        format_lsn(value),
        "0/0000000F",
        "padded and unpadded renderings are indistinguishable here, so the \
         server comparison could not detect padding"
    );
}
