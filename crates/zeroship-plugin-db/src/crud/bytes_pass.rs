//! Write-side codec for plain (non-encrypted) `t.bytes()` columns.
//!
//! THE DEFECT THIS EXISTS TO CLOSE. `t.bytes()` is exchanged with JS as a
//! base64 STRING (`sdks/db/src/types.ts`: `bytes(): TypeBuilder<string>`), and
//! the READ path has always honoured that: `column_to_json`'s OID-17 arm
//! base64-encodes the raw BYTEA bytes, and `read_pipeline::normalize_bytes_value`
//! passes that string through. The WRITE path had no `bytes` branch at all, so
//! the base64 string was bound as a plain text param at a BYTEA column and
//! Postgres parsed it in ESCAPE format - storing the 8 ASCII characters
//! `3q2+7w==` instead of the 4 bytes `de ad be ef` they encode. Measured on the
//! server before the fix, not inferred:
//!
//! ```text
//! SELECT encode(payload_bytes,'hex') FROM ... -> 3371322b37773d3d   (8 bytes)
//! ```
//!
//! which is the hex of the ASCII of the base64. The read path then base64'd
//! those 8 bytes, so `env.db` handed the caller `M3EyKzd3PT0=` - the base64 of
//! the base64. Fixing the READ path to un-double-encode would have left the
//! corrupted bytes on disk; the encode is what was wrong, so this pass sits on
//! the write side.
//!
//! SQLITE WAS WRONG TOO, ONLY INVISIBLY. rusqlite bound the same string as TEXT
//! into a BLOB-affinity column and read it back as TEXT, which
//! `normalize_bytes_value` passes through untouched - so the JS round trip
//! looked right while the stored cell held text, not bytes. That is the control
//! that isolates the layer: the read pipeline, the SDK and the JSON wire shape
//! are shared between the dialects and only the bind differs, so the bind is
//! where the bug was. This pass therefore runs on BOTH dialects.
//!
//! HOW IT LOWERS. The value the SDK hands us is base64 text and the column
//! wants raw bytes, which is exactly the binary-bind channel
//! `crud::encryption_pass` already uses for ciphertext:
//!
//! - Postgres: leave the (canonically re-encoded) base64 in the param list and
//!   deposit a `__zsbin__<col>` marker so the SQL builders emit
//!   `decode($N, 'base64')::bytea` at that placeholder.
//! - SQLite: rewrite the value to a `SQLITE_BINARY_BIND_PREFIX`-tagged param so
//!   the session actor binds a raw `Vec<u8>` as a BLOB.
//!
//! ENCRYPTED `bytes` COLUMNS ARE NOT OURS. `t.encrypted({ wraps: t.bytes() })`
//! is already handled end to end by `encryption_pass`, which base64-DECODES the
//! same wire string into the AEAD plaintext and re-marks the column with its
//! ciphertext. Touching those here would decode twice, so every walk below
//! skips a field def carrying `encrypted`.
//!
//! WHAT THIS DOES NOT COVER. Only WRITE documents and update patches. A FILTER
//! that names a `bytes` column (`find({ blob: "<base64>" })`) still sends the
//! base64 as a plain text param, so on Postgres it is compared against a BYTEA
//! and on SQLite against a BLOB - neither matches. That gap predates this pass
//! and is unchanged by it: `build_where_with_dialect_inner`, which lowers every
//! `find`/`update` filter, names `binary_bind` nowhere. The one filter builder
//! that DOES honour the marker is `build_conflict_probe_with_dialect`, and it
//! has exactly one caller - the deterministic-encryption upsert probe in
//! `write_pipeline`, which runs BEFORE this pass and so does not see its
//! markers either. Closing the gap means threading the schema into the filter
//! lowering, which is a different change from this one.

use base64::Engine as _;
use serde_json::Value;

use zeroship_data_core::error::DbError;
use crate::query::SqlDialect;

/// Cheap walk: does any field def on `schema` declare a plain, non-encrypted
/// `bytes` column? Drives the per-write decision to run this pass at all.
pub(crate) fn schema_has_plain_bytes_columns(schema: &Value) -> bool {
    schema
        .as_object()
        .map(|o| o.values().any(is_plain_bytes))
        .unwrap_or(false)
}

fn is_plain_bytes(def: &Value) -> bool {
    def.get("encrypted").is_none()
        && def.get("type").and_then(Value::as_str) == Some("bytes")
}

/// Field names on `schema` this pass owns, in schema order.
fn plain_bytes_fields(schema: &Value) -> Vec<String> {
    schema
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, def)| is_plain_bytes(def))
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Lower every plain `bytes` field of a write document (`insert`,
/// `insertMany` element, `upsert`) to the dialect's binary bind.
pub(crate) fn encode_bytes_on_write(
    schema: &Value,
    dialect: SqlDialect,
    doc: &mut Value,
) -> Result<(), DbError> {
    let fields = plain_bytes_fields(schema);
    if fields.is_empty() {
        return Ok(());
    }
    let Some(obj) = doc.as_object_mut() else {
        return Ok(());
    };
    let mut marks: Vec<String> = Vec::new();
    for field in &fields {
        if let Some(value) = obj.get_mut(field) {
            encode_bytes_scalar(field, dialect, value, &mut marks)?;
        }
    }
    for field in marks {
        obj.insert(format!("__zsbin__{field}"), Value::Bool(true));
    }
    Ok(())
}

/// Lower every plain `bytes` field of an update patch.
///
/// Both patch spellings the SQL builder accepts are covered: the nested
/// `{ "$set": { col: v } }` document and a top-level `{ col: v }` /
/// `{ col: { "$set": v } }` pair. The marker is deposited beside the value it
/// describes, because `build_set_clauses_with_dialect` unions the marker sets
/// from the top level AND from `$set`.
pub(crate) fn encode_bytes_on_update(
    schema: &Value,
    dialect: SqlDialect,
    patch: &mut Value,
) -> Result<(), DbError> {
    let fields = plain_bytes_fields(schema);
    if fields.is_empty() {
        return Ok(());
    }
    if let Some(set_doc) = patch.as_object_mut().and_then(|o| o.get_mut("$set")) {
        encode_bytes_on_write(schema, dialect, set_doc)?;
    }
    let Some(obj) = patch.as_object_mut() else {
        return Ok(());
    };
    let mut marks: Vec<String> = Vec::new();
    for field in &fields {
        let Some(value) = obj.get_mut(field) else {
            continue;
        };
        match value.as_object_mut().and_then(|ops| ops.get_mut("$set")) {
            Some(set_val) => encode_bytes_scalar(field, dialect, set_val, &mut marks)?,
            None => encode_bytes_scalar(field, dialect, value, &mut marks)?,
        }
    }
    for field in marks {
        obj.insert(format!("__zsbin__{field}"), Value::Bool(true));
    }
    Ok(())
}

/// Lower one scalar. `marks` collects the columns that still need a
/// `__zsbin__<col>` marker; the caller inserts them once the walk over the
/// object has finished borrowing it.
fn encode_bytes_scalar(
    field: &str,
    dialect: SqlDialect,
    value: &mut Value,
    marks: &mut Vec<String>,
) -> Result<(), DbError> {
    if value.is_null() {
        return Ok(());
    }
    // Idempotence guard: the value is already a lowered SQLite binary bind.
    // Reachable when an upsert re-runs the pass over a doc it has already
    // rewritten.
    if matches!(value, Value::String(s) if s.starts_with(crate::query::SQLITE_BINARY_BIND_PREFIX)) {
        return Ok(());
    }

    let Some(b64) = value.as_str() else {
        return Err(DbError::validation(
            "invalid_bytes_arg",
            format!(
                "db: bytes column '{field}' must be a base64-encoded string, got {}",
                json_kind(value)
            ),
        ));
    };
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| {
            DbError::validation(
                "invalid_bytes_arg",
                format!("db: bytes column '{field}' is not valid base64: {e}"),
            )
        })?;

    match dialect {
        SqlDialect::Sqlite => {
            *value = Value::String(super::sqlite_blob_param(&raw));
        }
        SqlDialect::Postgres | SqlDialect::Mysql => {
            // Re-encode from the bytes we just validated rather than forwarding
            // the caller's spelling. `decode(x, 'base64')` and Rust's STANDARD
            // engine do not accept the same inputs (PG skips newlines, we
            // reject them), so canonicalising here means the parameter the
            // database decodes is provably the value this pass approved.
            *value = Value::String(base64::engine::general_purpose::STANDARD.encode(&raw));
            marks.push(field.to_string());
        }
    }
    Ok(())
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The bytes every case below round-trips, stated as bytes rather than as
    /// an encoding of them.
    const RAW: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];

    fn b64() -> String {
        base64::engine::general_purpose::STANDARD.encode(RAW)
    }

    fn schema() -> Value {
        json!({
            "payload": { "type": "bytes" },
            "title": { "type": "string" }
        })
    }

    #[test]
    fn postgres_marks_the_column_and_keeps_canonical_base64() {
        let mut doc = json!({ "payload": b64(), "title": "t" });
        encode_bytes_on_write(&schema(), SqlDialect::Postgres, &mut doc).expect("encode");
        assert_eq!(doc["payload"], json!(b64()));
        assert_eq!(doc["__zsbin__payload"], json!(true));
        // A non-bytes sibling is untouched and unmarked.
        assert_eq!(doc["title"], json!("t"));
        assert!(doc.get("__zsbin__title").is_none());
    }

    #[test]
    fn sqlite_rewrites_the_value_to_a_blob_param_carrying_the_raw_bytes() {
        let mut doc = json!({ "payload": b64() });
        encode_bytes_on_write(&schema(), SqlDialect::Sqlite, &mut doc).expect("encode");
        let param = doc["payload"].as_str().expect("param string");
        let tagged = param
            .strip_prefix(crate::query::SQLITE_BINARY_BIND_PREFIX)
            .expect("blob sentinel");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(tagged)
                .expect("sentinel payload decodes"),
            RAW.to_vec()
        );
        // The SQLite arm needs no marker: the sentinel IS the side-channel.
        assert!(doc.get("__zsbin__payload").is_none());
    }

    #[test]
    fn an_encrypted_bytes_column_is_left_to_the_encryption_pass() {
        let schema = json!({
            "payload": {
                "type": "bytes",
                "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "bytes" }
            }
        });
        assert!(!schema_has_plain_bytes_columns(&schema));
        let mut doc = json!({ "payload": b64() });
        encode_bytes_on_write(&schema, SqlDialect::Postgres, &mut doc).expect("encode");
        assert!(doc.get("__zsbin__payload").is_none());
    }

    #[test]
    fn null_stays_null_and_is_not_marked() {
        let mut doc = json!({ "payload": Value::Null });
        encode_bytes_on_write(&schema(), SqlDialect::Postgres, &mut doc).expect("encode");
        assert_eq!(doc["payload"], Value::Null);
        assert!(doc.get("__zsbin__payload").is_none());
    }

    #[test]
    fn a_non_base64_string_is_a_typed_validation_error_not_a_silent_write() {
        let mut doc = json!({ "payload": "not base64!!" });
        let err = encode_bytes_on_write(&schema(), SqlDialect::Postgres, &mut doc)
            .expect_err("invalid base64 must be rejected");
        assert!(
            format!("{err:?}").contains("invalid_bytes_arg"),
            "expected a typed invalid_bytes_arg validation error, got {err:?}"
        );
    }

    #[test]
    fn a_non_string_value_is_rejected_rather_than_stringified() {
        let mut doc = json!({ "payload": [222, 173, 190, 239] });
        let err = encode_bytes_on_write(&schema(), SqlDialect::Postgres, &mut doc)
            .expect_err("a byte array is not the wire shape");
        assert!(
            format!("{err:?}").contains("invalid_bytes_arg"),
            "expected a typed invalid_bytes_arg validation error, got {err:?}"
        );
    }

    #[test]
    fn update_covers_nested_set_and_top_level_set_operator() {
        let mut nested = json!({ "$set": { "payload": b64() } });
        encode_bytes_on_update(&schema(), SqlDialect::Postgres, &mut nested).expect("encode");
        assert_eq!(nested["$set"]["__zsbin__payload"], json!(true));

        let mut op = json!({ "payload": { "$set": b64() } });
        encode_bytes_on_update(&schema(), SqlDialect::Postgres, &mut op).expect("encode");
        assert_eq!(op["__zsbin__payload"], json!(true));
        assert_eq!(op["payload"]["$set"], json!(b64()));

        let mut plain = json!({ "payload": b64() });
        encode_bytes_on_update(&schema(), SqlDialect::Postgres, &mut plain).expect("encode");
        assert_eq!(plain["__zsbin__payload"], json!(true));
    }

    #[test]
    fn lowering_is_idempotent_on_the_sqlite_arm() {
        let mut doc = json!({ "payload": b64() });
        encode_bytes_on_write(&schema(), SqlDialect::Sqlite, &mut doc).expect("first");
        let once = doc.clone();
        encode_bytes_on_write(&schema(), SqlDialect::Sqlite, &mut doc).expect("second");
        assert_eq!(doc, once);
    }
}
