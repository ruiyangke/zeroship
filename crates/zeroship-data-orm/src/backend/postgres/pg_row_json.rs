//! PostgreSQL row decoding into native values.
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

use zeroship_data_sql::value::Value;

/// Decode PostgreSQL rows into native records.
pub fn rows_to_values(rows: &[compio_postgres::Row]) -> Vec<Value> {
    rows.iter().map(row_to_value).collect()
}

/// Convert a single row to a native record.
///
/// Uses column OIDs to determine the native type:
/// - INT2/INT4/INT8 → number
/// - FLOAT4/FLOAT8 → number
/// - BOOL → boolean
/// - TEXT/VARCHAR → string
/// - UUID → string
/// - JSONB/JSON → parsed JSON value
/// - BYTEA → byte buffer
/// - TIMESTAMP/TIMESTAMPTZ/DATE → timestamp
/// - NUMERIC → exact decimal
/// - Other decodable text → string
pub(crate) fn row_to_value(row: &compio_postgres::Row) -> Value {
    let mut obj = zeroship_data_sql::value::Map::new();
    // Enumerate by index — compio_postgres's `Row::try_get(&str)` and
    // `Row::raw_value(&str)` resolve the name via a linear scan of
    // `row.columns()`, which makes `row_to_value` O(N²) in the column
    // count. Threading the index directly drops the per-column lookup
    // to O(1).
    for (idx, col) in row.columns().iter().enumerate() {
        let key = col.name().to_string();
        let value = column_to_value(row, idx, col.type_().oid());
        obj.insert(key, value);
    }
    Value::Object(obj)
}

/// Decode a single column into its native type based on its OID. Uses a
/// numeric column index (not the name) so `Row::try_get` /
/// `Row::raw_value` skip the linear name lookup — see the rationale
/// on `row_to_value` above.
///
/// `raw_value` refuses an index the row does not carry, and the arms below
/// fold that refusal into `Value::Null` alongside SQL NULL. That is safe
/// HERE and nowhere else: the only caller is `row_to_value`, which obtains
/// `idx` by enumerating `row.columns()`, so every index is in range by
/// construction and the refusal is unreachable. A caller resolving a column
/// BY NAME has no such guarantee and must propagate the error instead — see
/// `audit::read_processed_from_audit_row`.
fn column_to_value(row: &compio_postgres::Row, idx: usize, oid: u32) -> Value {
    // Try to get the value — if it's NULL, return null
    // OIDs from postgres_types::Type constants
    match oid {
        // BYTEA retains its native bytes.
        17 => match row.raw_value(idx) {
            Ok(Some(bytes)) => Value::Bytes(bytes.to_vec()),
            Ok(None) | Err(_) => Value::Null,
        },
        // BOOL = 16
        16 => match row.try_get::<_, bool>(idx) {
            Ok(v) => Value::Bool(v),
            Err(_) => Value::Null,
        },
        // INT2 = 21
        21 => match row.try_get::<_, i16>(idx) {
            Ok(v) => Value::Number(zeroship_data_sql::value::Number::from(v)),
            Err(_) => Value::Null,
        },
        // INT4 = 23
        23 => match row.try_get::<_, i32>(idx) {
            Ok(v) => Value::Number(zeroship_data_sql::value::Number::from(v)),
            Err(_) => Value::Null,
        },
        // INT8 = 20
        20 => match row.try_get::<_, i64>(idx) {
            Ok(v) => Value::Number(zeroship_data_sql::value::Number::from(v)),
            Err(_) => Value::Null,
        },
        // FLOAT4 = 700
        700 => match row.try_get::<_, f32>(idx) {
            Ok(v) => zeroship_data_sql::value::Number::from_f64(f64::from(v))
                .map_or(Value::Null, Value::Number),
            Err(_) => Value::Null,
        },
        // FLOAT8 = 701
        701 => match row.try_get::<_, f64>(idx) {
            Ok(v) => {
                zeroship_data_sql::value::Number::from_f64(v).map_or(Value::Null, Value::Number)
            }
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
                    Some(unix_ms) => Value::Timestamp(unix_ms),
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
                    Some(ms) => Value::Timestamp(ms),
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
        114 => row
            .raw_value(idx)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(bytes).ok())
            .unwrap_or(Value::Null),
        // NUMERIC is sent in PostgreSQL binary format, not UTF-8 text.
        1700 => row
            .raw_value(idx)
            .ok()
            .flatten()
            .and_then(decode_numeric)
            .map(Value::Decimal)
            .unwrap_or(Value::Null),
        // TEXT = 25, VARCHAR = 1043, CHAR = 18, BPCHAR = 1042, NAME = 19
        // and everything else: treat as text
        _ => match row.try_get::<_, String>(idx) {
            Ok(v) => Value::String(v),
            Err(_) => Value::Null,
        },
    }
}

// ---------------------------------------------------------------------------
// Bench entry points
// ---------------------------------------------------------------------------
//
// These two lived in `lib.rs` until 2026-09-01, which put
// `&compio_postgres::Row` into the signature of the crate advertised as a thin
// Rust/V8 seam - two always-compiled `pub fn`s, no `cfg`, so a build could not
// opt out of them. That is the same defect as the row decoders themselves being
// in `v8_bridge.rs`, one level up: the wrapper moved without the driver type
// moving with it.
//
// They are `pub` rather than `pub(crate)` because Criterion benches and the
// integration test link this crate as an EXTERNAL dependency and cannot reach
// `pub(crate)`. `lib.rs` re-exports both, so `zeroship_data_v8::…_for_bench`
// still resolves; a `pub use` names no type, so the adapter's signature surface
// stays vendor-free. When this module becomes `data-postgres`, the benches move
// with it and the re-export goes away.

/// **Bench-only**: thin wrapper around [`row_to_value`] so the `bench_row_to_json`
/// Criterion harness can measure native row decoding.
///
/// `#[doc(hidden)]` keeps it off the public docs surface.
/// `compio_postgres::test_utils::row_for_test` (doc-hidden there, and always
/// compiled) is the matching `Row` synthesiser - see
/// `crates/zeroship-data-v8/benches/bench_row_to_json.rs` for the wiring.
#[doc(hidden)]
#[must_use]
pub fn row_to_value_for_bench(row: &compio_postgres::Row) -> Value {
    row_to_value(row)
}

/// Benchmark the native first-row projection used before V8 materialization.
#[doc(hidden)]
#[must_use]
pub fn first_row_or_null_for_bench(rows: &[compio_postgres::Row]) -> Value {
    rows_to_values(rows)
        .into_iter()
        .next()
        .unwrap_or(Value::Null)
}

/// Decode finite PostgreSQL base-group numeric storage without floating point.
fn decode_numeric(bytes: &[u8]) -> Option<String> {
    use std::fmt::Write;
    if bytes.len() < 8 {
        return None;
    }
    let word = |offset| u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
    let count = usize::from(word(0));
    let weight = i32::from(word(2) as i16);
    let sign = word(4);
    let scale = usize::from(word(6));
    if bytes.len() != 8 + count * 2 || !matches!(sign, 0 | 0x4000) || scale > 16383 {
        return None;
    }
    let digits: Vec<_> = (0..count).map(|i| word(8 + i * 2)).collect();
    if digits.iter().any(|d| *d >= 10000) {
        return None;
    }
    let digit = |position: i32| {
        usize::try_from(weight - position)
            .ok()
            .and_then(|i| digits.get(i))
            .copied()
            .unwrap_or(0)
    };
    let mut result = String::new();
    if sign == 0x4000 && digits.iter().any(|d| *d != 0) {
        result.push('-');
    }
    if weight < 0 {
        result.push('0');
    } else {
        write!(result, "{}", digit(weight)).ok()?;
        for position in (0..weight).rev() {
            write!(result, "{:04}", digit(position)).ok()?;
        }
    }
    if scale > 0 {
        result.push('.');
        let start = result.len();
        for index in 1..=scale.div_ceil(4) {
            write!(result, "{:04}", digit(-(index as i32))).ok()?;
        }
        result.truncate(start + scale);
    }
    Some(result)
}
