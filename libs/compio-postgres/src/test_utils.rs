//! **Test-only** helpers for downstream crates (benches, unit tests)
//! that need to synthesise [`Row`] / [`Statement`] / [`Column`] values
//! without a live Postgres connection.
//!
//! Gated behind the `test-utils` Cargo feature so production builds
//! never see this surface. Added to unblock plugin-db's
//! `bench_row_to_json` ([I35] forcing function — performance r12); see
//! `crates/plugin-db/benches/bench_row_to_json.rs`.
//!
//! ## Why a builder, not just `Row::new`
//!
//! `Row::new` takes a [`DataRowBody`] whose fields are private inside
//! `postgres-protocol`; the only public path to one is feeding raw
//! wire bytes through [`postgres_protocol::message::backend::Message::parse`].
//! [`Statement::unnamed`] and [`Column`]'s fields are also `pub(crate)`.
//! This module wraps both — callers hand us column descriptors + raw
//! binary values and get back a `Row` that behaves exactly like one
//! produced by a real query (same `RowIndex` paths, same
//! `column_to_json` branches).

use crate::statement::{Column, Statement};
use crate::types::Type;
use crate::{Error, Row};
use bytes::BytesMut;
use postgres_protocol::message::backend::{DataRowBody, Message};

/// Construct a [`Column`] for tests. Mirrors the `pub(crate)` field
/// layout used internally; `table_oid` / `column_id` default to `None`
/// (the values Postgres sends for an ad-hoc expression with no
/// underlying table), and `type_modifier` defaults to `-1` (the
/// "no modifier present" sentinel `RowDescription` carries).
#[must_use]
pub fn column_for_test(name: impl Into<String>, ty: Type) -> Column {
    Column {
        name: name.into(),
        table_oid: None,
        column_id: None,
        type_modifier: -1,
        r#type: ty,
    }
}

/// Construct an unnamed [`Statement`] from a column list. Parameter
/// types default to empty — the bench harness only needs the column
/// metadata for `Row::columns()` / `Row::try_get` / `Row::raw_value`.
#[must_use]
pub fn statement_for_test(columns: Vec<Column>) -> Statement {
    Statement::unnamed(Vec::new(), columns)
}

/// Synthesise a [`Row`] from column descriptors and per-column raw
/// binary values (PostgreSQL binary wire format; `None` is SQL NULL).
///
/// Wire-format reminder: the values you pass here must match what
/// Postgres would send for the column's `Type` — e.g. `INT4` is a
/// 4-byte big-endian `i32`, `BOOL` is a single byte (0 or 1), `JSONB`
/// is a 1-byte version prefix (0x01) followed by UTF-8 JSON text,
/// `TIMESTAMPTZ` is an 8-byte BE `i64` of microseconds since
/// 2000-01-01 UTC. See `crates/plugin-db/src/v8_bridge.rs::column_to_json`
/// for the conversion table.
///
/// # Errors
///
/// Returns the same `Error` variants as a normal `Row::new` call would
/// (parse error if the synthesised buffer is malformed). The function
/// panics on `usize → i32 / u16 / u32` overflow because the test inputs
/// are bounded by what fits on a stack — overflow here would mean
/// something is deeply wrong with the test fixture.
pub fn row_for_test(
    columns: Vec<Column>,
    values: Vec<Option<Vec<u8>>>,
) -> Result<Row, Error> {
    assert_eq!(
        columns.len(),
        values.len(),
        "row_for_test: column / value count mismatch ({} vs {})",
        columns.len(),
        values.len(),
    );

    let statement = statement_for_test(columns);
    let body = build_data_row_body(&values);
    Row::new(statement, body)
}

/// Build a `DataRowBody` by writing a synthetic `DataRow` message
/// (tag 'D' + length + col_count + per-col [i32_len + bytes]) and
/// feeding it through `Message::parse`. This is the only way to
/// produce a `DataRowBody` from outside `postgres-protocol`.
fn build_data_row_body(values: &[Option<Vec<u8>>]) -> DataRowBody {
    // DataRow wire format:
    //   1 byte  : tag = b'D' (0x44)
    //   4 bytes : length (BE u32, includes the length field but not the tag)
    //   2 bytes : col_count (BE u16)
    //   per col : i32 BE length (-1 for NULL) + that many raw bytes
    let mut payload_len: usize = 2; // col_count
    for v in values {
        payload_len += 4; // i32 len
        if let Some(bytes) = v {
            payload_len += bytes.len();
        }
    }
    let total_len = 4 + payload_len; // length field includes itself
    let total_len_u32: u32 = total_len.try_into().expect("DataRow length fits in u32");
    let col_count_u16: u16 = values
        .len()
        .try_into()
        .expect("DataRow column count fits in u16");

    let mut buf = BytesMut::with_capacity(1 + total_len);
    buf.extend_from_slice(&[b'D']);
    buf.extend_from_slice(&total_len_u32.to_be_bytes());
    buf.extend_from_slice(&col_count_u16.to_be_bytes());
    for v in values {
        match v {
            None => buf.extend_from_slice(&(-1_i32).to_be_bytes()),
            Some(bytes) => {
                let len_i32: i32 = bytes
                    .len()
                    .try_into()
                    .expect("DataRow column length fits in i32");
                buf.extend_from_slice(&len_i32.to_be_bytes());
                buf.extend_from_slice(bytes);
            }
        }
    }

    match Message::parse(&mut buf).expect("synthetic DataRow parses") {
        Some(Message::DataRow(body)) => body,
        Some(_) => panic!("synthetic DataRow buffer parsed as a non-DataRow message"),
        None => panic!("synthetic DataRow buffer underflowed Message::parse"),
    }
}
