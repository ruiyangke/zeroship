//! Session settings that change how PostgreSQL RENDERS values, and why they
//! cannot reach a decoded value here.
//!
//! Every `Bind` this driver sends asks for BINARY results -- `encode_bind_raw`
//! passes `Some(1)`, and the two text-parameter paths pass `once(1i16)`. Binary
//! output is not affected by `DateStyle`, `bytea_output`, `extra_float_digits`
//! or `IntervalStyle`, so a session that changes any of them cannot corrupt a
//! typed value the caller reads.
//!
//! That is a real robustness property and nothing tested it. It is also the
//! kind that regresses invisibly: switching one `Bind` to text results for
//! debugging would make every date, float and bytea start depending on session
//! state, and every existing test would still pass because the defaults are
//! the ones everyone develops against.
//!
//! THE CONTROL IS THE POINT. Each case below sets a GUC to a hostile value and
//! then reads the SAME value two ways: through `simple_query`, which is the
//! text protocol and DOES change, and through the extended path, which must
//! not. If the driver ever stopped requesting binary results, the two would
//! agree and this test would fail -- whereas asserting only that the extended
//! path is correct would still pass, because it would be correct by accident
//! on a default server.

use compio_postgres::{Client, SimpleQueryMessage};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const WATCHDOG: Duration = Duration::from_secs(30);

fn test_url() -> String {
    common::test_url()
}

async fn connect() -> Client {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

/// The one text-protocol rendering of `sql`, for use as the control.
async fn rendered(client: &Client, sql: &str) -> String {
    for message in client.simple_query(sql).await.expect("simple_query") {
        if let SimpleQueryMessage::Row(row) = message {
            return row.get(0).expect("one column").to_string();
        }
    }
    panic!("no row from {sql}");
}

#[compio::test]
async fn rendering_gucs_cannot_reach_a_binary_decoded_value() {
    compio::time::timeout(WATCHDOG, async {
        let client = connect().await;

        // ---- bytea_output ----
        const BYTEA: &str = "SELECT '\\x00ff41'::bytea";
        let hex_render = rendered(&client, BYTEA).await;
        client
            .batch_execute("SET bytea_output TO 'escape'")
            .await
            .expect("set bytea_output");
        let escape_render = rendered(&client, BYTEA).await;
        assert_ne!(
            hex_render, escape_render,
            "bytea_output did not change the text rendering, so this test cannot \
             tell a binary result from a text one"
        );
        let decoded: Vec<u8> = client
            .query_one(BYTEA, &[])
            .await
            .expect("bytea query")
            .get(0);
        assert_eq!(
            decoded,
            vec![0x00, 0xff, 0x41],
            "bytea_output reached a decoded value, so results are not binary"
        );

        // ---- extra_float_digits ----
        const FLOAT: &str = "SELECT 0.1::float8 + 0.2::float8";
        let full_render = rendered(&client, FLOAT).await;
        client
            .batch_execute("SET extra_float_digits TO -3")
            .await
            .expect("set extra_float_digits");
        let short_render = rendered(&client, FLOAT).await;
        assert_ne!(
            full_render, short_render,
            "extra_float_digits did not change the text rendering"
        );
        let value: f64 = client
            .query_one(FLOAT, &[])
            .await
            .expect("float query")
            .get(0);
        // The exact double, not the rounded rendering the session now asks for.
        assert!(
            (value - 0.30000000000000004).abs() < f64::EPSILON,
            "extra_float_digits reached a decoded value: {value}"
        );

        // ---- DateStyle ----
        const DATE: &str = "SELECT '2026-08-23'::date";
        let iso_render = rendered(&client, DATE).await;
        client
            .batch_execute("SET DateStyle TO 'German, DMY'")
            .await
            .expect("set DateStyle");
        let german_render = rendered(&client, DATE).await;
        assert_ne!(
            iso_render, german_render,
            "DateStyle did not change the text rendering"
        );
        // Read back as text through the EXTENDED path: the server still sends
        // the binary date, and the driver renders it itself.
        let same: String = client
            .query_one("SELECT ('2026-08-23'::date)::text", &[])
            .await
            .expect("date query")
            .get(0);
        assert_eq!(
            same, german_render,
            "a ::text cast is rendered by the SERVER, so it should follow DateStyle"
        );
    })
    .await
    .expect("session-GUC test exceeded its watchdog");
}
