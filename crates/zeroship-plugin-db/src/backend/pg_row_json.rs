//! PostgreSQL row -> JSON decoding.
//!
//! Driver logic: it reads `compio_postgres::Row` and its column OIDs, and it
//! belongs to the Postgres adapter.
//!
//! This lived in `v8_bridge.rs` until 2026-08-31, which made the Postgres
//! backend call UP into the V8 adapter to decode its own rows - one adapter
//! depending on a different adapter, and the reason a crate advertised as a
//! thin Rust/V8 seam linked the Postgres driver.
//!
//! Nothing here knows about V8, and nothing here may.

use base64::Engine as _;
use serde_json::Value;

/// Convert rows to a `Vec<serde_json::Value>` — one `Value::Object`
/// per row, in result order.
///
/// This is the typed intermediate the exec layer threads from
/// `compio_postgres::Row` through to the V8 boundary. Callers that
/// need the final JSON-array string serialise once at the boundary
/// (`Value::Array(rows_to_json_value(&rows)).to_string()`); the
/// intermediate `Vec<Value>` lets the CRUD resolvers
/// (`first_row_or_null`, `row_count_as_f64`) inspect or take a single
/// row without paying for a JSON parse + reserialise of the whole
/// result set.
pub(crate) fn rows_to_json_value(rows: &[compio_postgres::Row]) -> Vec<Value> {
    rows.iter().map(row_to_json).collect()
}

/// Convert a single Row to a JSON object.
///
/// Uses column OIDs to determine the appropriate JSON type:
/// - INT2/INT4/INT8 → number
/// - FLOAT4/FLOAT8 → number
/// - BOOL → boolean
/// - TEXT/VARCHAR → string
/// - UUID → string
/// - JSONB/JSON → parsed JSON value
/// - Everything else → string (via text representation)
pub(crate) fn row_to_json(row: &compio_postgres::Row) -> Value {
    let mut obj = serde_json::Map::new();
    // Enumerate by index — compio_postgres's `Row::try_get(&str)` and
    // `Row::raw_value(&str)` resolve the name via a linear scan of
    // `row.columns()`, which makes `row_to_json` O(N²) in the column
    // count. Threading the index directly drops the per-column lookup
    // to O(1).
    for (idx, col) in row.columns().iter().enumerate() {
        let key = col.name().to_string();
        let value = column_to_json(row, idx, col.type_().oid());
        obj.insert(key, value);
    }
    Value::Object(obj)
}

/// Convert a single column value to JSON based on its OID. Uses a
/// numeric column index (not the name) so `Row::try_get` /
/// `Row::raw_value` skip the linear name lookup — see the rationale
/// on `row_to_json` above.
///
/// `raw_value` refuses an index the row does not carry, and the arms below
/// fold that refusal into `Value::Null` alongside SQL NULL. That is safe
/// HERE and nowhere else: the only caller is `row_to_json`, which obtains
/// `idx` by enumerating `row.columns()`, so every index is in range by
/// construction and the refusal is unreachable. A caller resolving a column
/// BY NAME has no such guarantee and must propagate the error instead — see
/// `audit::read_processed_from_audit_row`.
fn column_to_json(row: &compio_postgres::Row, idx: usize, oid: u32) -> Value {
    // Try to get the value — if it's NULL, return null
    // OIDs from postgres_types::Type constants
    match oid {
        // BYTEA = 17 — canonical wire shape is base64 text.
        17 => match row.raw_value(idx) {
            Ok(Some(bytes)) => {
                Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            Ok(None) | Err(_) => Value::Null,
        },
        // BOOL = 16
        16 => match row.try_get::<_, bool>(idx) {
            Ok(v) => Value::Bool(v),
            Err(_) => Value::Null,
        },
        // INT2 = 21
        21 => match row.try_get::<_, i16>(idx) {
            Ok(v) => Value::Number(serde_json::Number::from(v)),
            Err(_) => Value::Null,
        },
        // INT4 = 23
        23 => match row.try_get::<_, i32>(idx) {
            Ok(v) => Value::Number(serde_json::Number::from(v)),
            Err(_) => Value::Null,
        },
        // INT8 = 20
        20 => match row.try_get::<_, i64>(idx) {
            Ok(v) => Value::Number(serde_json::Number::from(v)),
            Err(_) => Value::Null,
        },
        // FLOAT4 = 700
        700 => match row.try_get::<_, f32>(idx) {
            Ok(v) => serde_json::Number::from_f64(f64::from(v)).map_or(Value::Null, Value::Number),
            Err(_) => Value::Null,
        },
        // FLOAT8 = 701
        701 => match row.try_get::<_, f64>(idx) {
            Ok(v) => serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number),
            Err(_) => Value::Null,
        },
        // UUID = 2950
        2950 => match row.try_get::<_, uuid::Uuid>(idx) {
            Ok(v) => Value::String(v.to_string()),
            Err(_) => Value::Null,
        },
        // TIMESTAMP = 1114, TIMESTAMPTZ = 1184
        // Postgres sends as i64 microseconds since 2000-01-01 00:00:00 UTC.
        // Return as Unix milliseconds (number) — matches JS Date.now() / new Date(ts).
        // Postgres `infinity`/`-infinity` arrive as `i64::MAX`/`i64::MIN`;
        // wrap arithmetic in `checked_*` so an overflowing sentinel becomes
        // `null` (the conceptual `DbError::Internal`) instead of panicking
        // the worker thread.
        1114 | 1184 => match row.raw_value(idx) {
            Ok(Some(bytes)) if bytes.len() == 8 => {
                let pg_usec = i64::from_be_bytes(bytes.try_into().unwrap());
                // 2000-01-01 = 946684800 seconds since Unix epoch
                match (pg_usec / 1_000).checked_add(946_684_800_000) {
                    Some(unix_ms) => Value::Number(serde_json::Number::from(unix_ms)),
                    None => {
                        tracing::warn!(
                            oid = oid,
                            pg_usec = pg_usec,
                            "db: TIMESTAMP arithmetic overflow (likely Postgres ±infinity); returning null"
                        );
                        Value::Null
                    }
                }
            }
            _ => Value::Null,
        },
        // DATE = 1082 — i32 days since 2000-01-01
        // Return as Unix milliseconds at midnight UTC.
        // Same overflow concern as TIMESTAMP: `infinity` arrives as
        // `i32::MAX`, which multiplies past `i64::MAX`. Checked math
        // turns the overflow into `null` rather than a panic.
        1082 => match row.raw_value(idx) {
            Ok(Some(bytes)) if bytes.len() == 4 => {
                let pg_days = i32::from_be_bytes(bytes.try_into().unwrap());
                let unix_ms = i64::from(pg_days)
                    .checked_add(10957)
                    .and_then(|d| d.checked_mul(86_400_000));
                match unix_ms {
                    Some(ms) => Value::Number(serde_json::Number::from(ms)),
                    None => {
                        tracing::warn!(
                            oid = oid,
                            pg_days = pg_days,
                            "db: DATE arithmetic overflow (likely Postgres ±infinity); returning null"
                        );
                        Value::Null
                    }
                }
            }
            _ => Value::Null,
        },
        // JSONB = 3802 — binary format has 1-byte version prefix, strip it
        3802 => match row.raw_value(idx) {
            Ok(Some(bytes)) if bytes.len() > 1 => {
                let json_str = std::str::from_utf8(&bytes[1..]).unwrap_or("null");
                serde_json::from_str(json_str).unwrap_or(Value::Null)
            }
            _ => Value::Null,
        },
        // JSON = 114 — text format, no prefix
        114 => match row.try_get::<_, String>(idx) {
            Ok(s) => {
                let parsed = serde_json::from_str(&s).ok();
                parsed.unwrap_or(Value::String(s))
            }
            Err(_) => Value::Null,
        },
        // NUMERIC = 1700 — Postgres' arbitrary-precision decimal. Map to
        // a JSON number when it fits exactly; fall back to string (lossy
        // float would corrupt big decimals). The SDK's `t.number()` maps
        // to NUMERIC, so user-facing `doc.field` should be a number, not
        // a string. Postgres serialises NUMERIC over the text protocol
        // as a decimal string; parse it.
        1700 => match row.try_get::<_, String>(idx) {
            Ok(s) => {
                if let Ok(i) = s.parse::<i64>() {
                    Value::Number(serde_json::Number::from(i))
                } else if let Ok(f) = s.parse::<f64>() {
                    serde_json::Number::from_f64(f).map_or(Value::String(s), Value::Number)
                } else {
                    Value::String(s)
                }
            }
            Err(_) => Value::Null,
        },
        // TEXT = 25, VARCHAR = 1043, CHAR = 18, BPCHAR = 1042, NAME = 19
        // and everything else: treat as text
        _ => match row.try_get::<_, String>(idx) {
            Ok(v) => Value::String(v),
            Err(_) => Value::Null,
        },
    }
}
