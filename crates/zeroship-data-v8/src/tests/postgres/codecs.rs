//! PostgreSQL codecs contracts.
use super::fixtures::*;

use crate::tests::fixtures::parity;

use compio_postgres::NoTls;

use zeroship_data_sql::value::value;

/// A `t.bytes()` value written through `env.db` must reach Postgres AS BYTES.
///
/// THE SDK IS NOT ALLOWED TO BE ITS OWN WITNESS HERE. `parity_matrix_*` above
/// compares what `env.db` reads back against what `env.db` was given, and that
/// pair was self-consistent all through the defect on SQLite: a value stored as
/// TEXT and read back as TEXT round-trips perfectly while the cell holds the
/// wrong thing. So this test goes around the SDK entirely and asks the server
/// what is in the column.
///
/// RED BEFORE THE FIX, and measured that way rather than assumed: against the
/// pre-fix binary the stored cell is `\x3371322b37773d3d`, the 8-byte ASCII of
/// the base64 `3q2+7w==`, and this assertion fails naming both. After
/// `crud::bytes_pass` it is `\xdeadbeef`.
#[compio::test]
async fn bytes_column_stores_raw_bytes_on_postgres() {
    let (_postgres, pg_url) = require_pg().await;
    let app = crate::tests::fixtures::test_app_id!();
    let pg = parity::run_matrix(&pg_url, &app);

    // The expectation is DERIVED, not copied from a run: `TYPED_BYTES_RAW` is
    // what the caller handed `env.db` (base64-encoded, per the `t.bytes()` wire
    // contract), so it is what the column must hold.
    let expected: Vec<u8> = parity::TYPED_BYTES_RAW.to_vec();

    let (client, connection) = compio_postgres::connect(&pg_url, NoTls)
        .await
        .expect("dial the parity database directly");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let sql = format!(
        "SELECT payload_bytes FROM \"{}\".\"{}\" WHERE title = $1",
        app, pg.collection
    );
    let rows = client
        .query(&sql, &[&"typed-roundtrip"])
        .await
        .expect("read the stored cell");
    assert_eq!(rows.len(), 1, "the typed round-trip row must exist");
    let stored: Vec<u8> = rows[0].get::<_, Vec<u8>>(0);

    // Hand the socket back BEFORE the assertions: a panic skips whatever
    // follows it, and `direct_connection_sites_do_not_grow` counts this site on
    // the promise that it is paired with a teardown.
    drop(client);
    drain_pg().await;

    assert_eq!(
        stored,
        expected,
        "the bytea cell must hold the caller's bytes. Got {} bytes ({}), wanted \
         {} ({}). An 8-byte cell spelling the base64 in ASCII is the write path \
         binding the base64 string as text at a bytea column.",
        stored.len(),
        hex_of(&stored),
        expected.len(),
        hex_of(&expected),
    );

    // And the value the caller reads back through `env.db` is the base64 of
    // exactly those bytes - one encode, not two.
    //
    // INDEX AT THE LEVEL `typed` IS BUILT AT. `run_matrix` stores the whole
    // `typedRoundTrip` return value, which is `{ source, echo }` - two
    // projected rows (`parity/mod.rs`, `typedRoundTrip` returns
    // `{ source: projectTypedRow(source), echo: ... }`). `payload_bytes` lives
    // one level down inside each. A bare `pg.typed["payload_bytes"]` is
    // therefore `Value::Null` WHATEVER the product does - it named a key the
    // map does not have - and that is exactly how this assertion failed from
    // the day it was written: `left: Null, right: String("3q2+7w==")`. It could
    // not have gone green for a correct product or red for a broken one.
    for row in ["source", "echo"] {
        assert_eq!(
            pg.typed[row]["payload_bytes"],
            value!(parity::TYPED_BYTES_RAW),
            "env.db must hand back the base64 of the stored bytes on the `{row}` \
             row; got {:?} in {:?}",
            pg.typed[row]["payload_bytes"],
            pg.typed,
        );
    }
}

/// Render bytes as lowercase hex for the failure messages above. Not a helper
/// worth a crate: `format!("{:02x?}")` prints a debug list, not a hex string.
fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
