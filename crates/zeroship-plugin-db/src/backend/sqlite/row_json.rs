//! SQLite typed-row -> JSON decoding.
//!
//! The peer of `super::super::pg_row_json`, and deliberately NOT shared with
//! it: PostgreSQL decodes wire bytes by OID in one pass, while SQLite has
//! already materialised `TypedRows` because rusqlite rows cannot cross the
//! actor's thread hop. Unifying them would need a fattened universal cell and
//! an extra allocation pass on every Postgres row, which is the hot path.
//!
//! What the two DO share is the output contract - object per row, base64
//! STANDARD for bytes, non-finite floats folded to null - and that is pinned by
//! the parity suite, which runs both backends against one golden projection.
//!
//! Moved out of `v8_bridge.rs` on 2026-08-31: the SQLite backend was calling up
//! into the V8 adapter to decode its own rows.

use base64::Engine as _;
use serde_json::Value;

use super::session::{TypedCell, TypedRows};

/// Convert SQLite typed rows into the same JSON shape the PG row
/// decoder emits.
///
/// BLOBs surface as base64 strings. The CRUD read-side normalizer
/// consults the declared schema after this step; this helper's job is
/// only to preserve bytes losslessly across the JSON boundary.
pub(crate) fn typed_rows_to_json_value(rows: &TypedRows) -> Vec<Value> {
    rows.rows
        .iter()
        .map(|row| Value::Object(typed_row_to_json_object(&rows.columns, row)))
        .collect()
}

pub(crate) fn typed_row_to_json_object(
    columns: &[String],
    row: &[TypedCell],
) -> serde_json::Map<String, Value> {
    let mut obj = serde_json::Map::with_capacity(columns.len());
    for (idx, name) in columns.iter().enumerate() {
        let value = row.get(idx).map(typed_cell_to_json).unwrap_or(Value::Null);
        obj.insert(name.clone(), value);
    }
    obj
}

fn typed_cell_to_json(cell: &TypedCell) -> Value {
    match cell {
        TypedCell::Null => Value::Null,
        TypedCell::Integer(n) => Value::Number(serde_json::Number::from(*n)),
        TypedCell::Real(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
        TypedCell::Text(s) => Value::String(s.clone()),
        TypedCell::Blob(bytes) => {
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
    }
}
