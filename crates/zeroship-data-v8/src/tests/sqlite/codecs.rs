//! SQLite codecs contracts.
use super::fixtures::*;

use crate::tests::fixtures::parity;

#[test]
fn exact_decimal_strings_round_trip_through_v8() {
    run(async {
        let dir = tempfile::tempdir().expect("create decimal fixture dir");
        apply_schema_ahead_of_runtime(
            &dir,
            &format!(
                r#"CREATE TABLE "{LOCAL_DEV_APP_ID}"."ledger" ({SYSTEM_COLUMNS_SQLITE}, "amount" TEXT NOT NULL);"#
            ),
        );
        let schema = zeroship_data_orm::value!({
            "amount": {"type": "number", "precision": 30, "scale": 2, "required": true}
        });
        let source = sqlite_runtime_source(
            "ledger",
            &schema,
            r#"
const _procedures = {
  async decimalRoundTrip() {
    const ledger = env.db.collection(COLLECTION);
    const inserted = await ledger.insert({ amount: "9007199254740993.00" });
    const updated = await ledger.update(
      { amount: "9007199254740993.000" },
      { amount: { $inc: "0.01" } },
    );
    const found = await ledger.find({ amount: { $in: ["9007199254740993.010"] } });
    let lossyNumberCode = null;
    try {
      await ledger.insert({ amount: 1.25 });
    } catch (error) {
      lossyNumberCode = error.code;
    }
    return {
      inserted: inserted.amount,
      updated: updated.amount,
      found: found.map(row => row.amount),
      valueType: typeof updated.amount,
      lossyNumberCode,
    };
  },
};
"#,
        );

        let response = dispatch_sqlite_runtime(&dir, &source, "decimalRoundTrip");
        assert_eq!(
            response,
            zeroship_data_orm::value!({
                "json": {
                    "inserted": "9007199254740993.00",
                    "updated": "9007199254740993.01",
                    "found": ["9007199254740993.01"],
                    "valueType": "string",
                    "lossyNumberCode": "invalid_typed_value"
                }
            })
        );
    });
}

/// The dev tier must store a `t.bytes()` value as a BLOB of the caller's bytes.
///
/// WHY THIS IS SEPARATE FROM THE PROJECTION TEST ABOVE, and why SQLite needed a
/// test at all. Before `crud::bytes_pass`, the projection test above was GREEN
/// while the stored cell was wrong: rusqlite bound the SDK's base64 string as
/// TEXT into a BLOB-affinity column, read it back as TEXT, and
/// `read_pipeline::normalize_bytes_value` passes a string through untouched - so
/// the input reappeared and the round trip looked perfect. `typeof()` is what
/// separates the two, and it is the reason the SQLite leg is the control that
/// isolates the layer: the SDK, the read pipeline and the JSON wire shape are
/// shared with Postgres, so a defect visible on one and hidden on the other has
/// to live below them, in the bind.
#[test]
fn bytes_column_stores_a_raw_blob_on_sqlite() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);

        // Re-attach the matrix's app database before using its qualified name.
        let client = crate::tests::fixtures::sqlite::Inspector::open(dir.path());
        // `query` materialises every cell as `Option<String>` and renders a BLOB
        // as `<N bytes blob>`, so ask SQLite itself for the discriminant and the
        // hex - the same route `p5_*` uses for ciphertext.
        let sql = format!(
            "SELECT typeof(payload_bytes), hex(payload_bytes) FROM \"{LOCAL_DEV_APP_ID}\".\"{}\" \
             WHERE title = 'typed-roundtrip'",
            snapshot.collection
        );
        let rows = client.query(&sql, &[]).await.expect("SELECT");
        assert_eq!(rows.len(), 1, "the typed round-trip row must exist");

        let kind = rows[0][0].clone().expect("typeof() is never null");
        let hex = rows[0][1].clone().expect("hex() is never null");
        let expected_hex: String = parity::TYPED_BYTES_RAW
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();

        assert_eq!(
            kind, "blob",
            "a t.bytes() cell must be a BLOB, not {kind}. 'text' here is the \
             write path binding the base64 wire string as text into a \
             BLOB-affinity column, which round-trips through env.db while \
             storing the wrong thing"
        );
        assert_eq!(hex, expected_hex, "the BLOB must hold the caller's bytes");
    });
}
