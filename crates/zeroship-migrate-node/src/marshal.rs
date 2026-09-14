//! napi <-> `zeroship_migrate` driver-type marshaling.
//!
//! The host driver (`pg`/`mysql2` in JS) speaks JS cells; the engine speaks the
//! driver-neutral [`Bind`]/[`Value`]/[`Row`]/[`DbError`] types. This module is the
//! ONLY place those two representations meet. The DTOs the fold operates over
//! ([`JsCell`]/[`JsRow`]/[`JsReply`]/[`JsError`]/[`JsRequest`]) live in
//! [`crate::wire`] - the single source of truth for every N-API boundary type - and
//! are re-exported here for the fold + the mock-apply test.
//!
//! Value union (verified exhaustively): `Null | Text | Int | Bool | TextArray`.
//! Ints cross as JS strings when they exceed the safe-integer domain? - NO: the
//! engine's int domain is `i64`, but the ONLY ints the seam reads are small
//! catalog/count values (`character_maximum_length`, `relkind`-as-char, row
//! counts). We carry `Int` as an `f64` on the JS side (a JS `number`) for the
//! small values the seam actually reads, and additionally accept an `i64`-as-string
//! form (`intStr`) so a host `pg` type-parser that stringifies `int8`/`numeric`
//! round-trips exactly. `Text` covers the `to_char` timestamp
//! and all text/name/varchar cells.

use zeroship_migrate::driver::{Bind, DbError, Row, Value};

pub use crate::wire::{JsCell, JsError, JsReply, JsRequest, JsRow};

// ---------------------------------------------------------------------------
// Conversions - the neutral <-> JS-cell fold. Pure functions, no napi types, so
// this whole module compiles WITHOUT the `napi` feature and the mock-apply
// integration test exercises the folds directly.
// ---------------------------------------------------------------------------

/// `Bind -> JsCell` (Rust -> JS bind fold). `Int->int`, `Text->text`,
/// `Bool->bool`, `Null->null`, `Decimal->text` (a decimal crosses every seam as its
/// canonical string, and PG infers the numeric target from context).
///
/// # Errors
/// Returns a message naming the variant if `bind` is one this fold does not
/// handle. See the fallback arm: a bind that cannot be represented is REFUSED,
/// never silently sent as NULL.
pub fn bind_to_cell(bind: &Bind) -> Result<JsCell, String> {
    Ok(match bind {
        Bind::Null => cell_null(),
        Bind::Bool(b) => cell_bool(*b),
        // i64 binds only ever carry the small `exec_ms` today; cross as a string
        // so no precision is lost even if a future large bind appears.
        Bind::Int(n) => cell_int_str(n.to_string()),
        Bind::Decimal(s) => cell_text(s.clone()),
        Bind::Text(s) => cell_text(s.clone()),
        // `Inferred` folds to the SAME cell as `Text`, and that is correct rather
        // than lossy: this wire carries no declared types at all. Every cell
        // reaches node-pg / mysql2 as a plain JS value and both send it
        // text-format with no OID, so on this path every bind is already inferred.
        // The distinction only has teeth for a driver that declares types, which
        // is an in-process Rust one, not a host across this bridge.
        Bind::Inferred(Some(s)) => cell_text(s.clone()),
        Bind::Inferred(None) => cell_null(),
        // `Bind` is `#[non_exhaustive]`, so this arm cannot be deleted: the enum's
        // own docs say the wildcard is what "lets a driver author keep a wildcard
        // arm for future additions". All six of today's variants are handled above,
        // so it is unreachable AT PRESENT - and that is exactly why what it does
        // matters, because the day it becomes reachable is the day nobody is
        // looking.
        //
        // UNTIL 2026-09-04 IT RETURNED `cell_null()`, and the comment called it
        // unreachable in practice. That made an unhandled bind a SILENT SQL NULL:
        // a value the engine asked to be written would be written as NULL, with no
        // error, no log line and no failed migration - the write would report
        // success. On a data-fidelity path that is the worst available outcome. It
        // is strictly worse than a crash, because a crash is seen.
        //
        // The assurance was not idle speculation either. This session proved the
        // analogous "cannot happen" claim FALSE for two other drivers, where
        // `Bind::Bool` and `Bind::Decimal` turned out to be emitted by live code
        // paths that a shared conformance suite had never bound.
        //
        // So: refuse. `call` in session.rs already returns `Result<_, DbError>`,
        // so this costs one `?` and the failure surfaces as an ordinary seam error.
        other => return Err(format!(
            "bind_to_cell: unhandled Bind variant {other:?}. `Bind` is #[non_exhaustive]; \
             a variant was added engine-side without teaching this fold how to carry it. \
             REFUSING rather than sending SQL NULL, which would silently corrupt the write."
        )),
    })
}

/// `JsCell -> Value` (JS -> Rust return fold). The `int`/`intStr` split:
/// `intStr` is preferred (exact int8/numeric), falling back to the `f64` `int`
/// narrowed to `i64`.
///
/// # Errors
/// Returns a message describing the malformed cell if `kind` is unknown or the
/// selected payload field is absent / unparseable.
pub fn cell_to_value(cell: &JsCell) -> Result<Value, String> {
    match cell.kind.as_str() {
        "null" => Ok(Value::Null),
        "text" => cell
            .text
            .clone()
            .map(Value::Text)
            .ok_or_else(|| "text cell missing `text` payload".to_string()),
        "int" => {
            if let Some(s) = &cell.int_str {
                s.parse::<i64>()
                    .map(Value::Int)
                    .map_err(|_| format!("int cell `intStr` not an i64: {s:?}"))
            } else if let Some(f) = cell.int {
                // A JS number carrying an integer; reject a non-integral or
                // out-of-range value rather than silently truncating.
                if f.fract() != 0.0 || f.abs() >= 9.007_199_254_740_992e15 {
                    Err(format!("int cell `int` not a safe integer: {f}"))
                } else {
                    Ok(Value::Int(f as i64))
                }
            } else {
                Err("int cell missing both `int` and `intStr`".to_string())
            }
        }
        "bool" => cell
            .bool
            .map(Value::Bool)
            .ok_or_else(|| "bool cell missing `bool` payload".to_string()),
        "textArray" => cell
            .text_array
            .clone()
            .map(Value::TextArray)
            .ok_or_else(|| "textArray cell missing `textArray` payload".to_string()),
        other => Err(format!("unknown cell kind: {other:?}")),
    }
}

/// `JsRow -> Row`.
///
/// # Errors
/// Returns a message if any cell is malformed or columns/cells length-mismatch.
pub fn row_to_seam(row: &JsRow) -> Result<Row, String> {
    if row.columns.len() != row.cells.len() {
        return Err(format!(
            "row columns/cells length mismatch: {} vs {}",
            row.columns.len(),
            row.cells.len()
        ));
    }
    let values = row
        .cells
        .iter()
        .map(cell_to_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Row::new(row.columns.clone(), values))
}

/// `JsError -> DbError`.
#[must_use]
pub fn js_error_to_seam(e: &JsError) -> DbError {
    DbError {
        message: e.message.clone(),
        sqlstate: e.code.clone(),
    }
}

// --- JsCell constructors (also used by the mock-apply integration test) ---

#[must_use]
pub fn cell_null() -> JsCell {
    JsCell {
        kind: "null".into(),
        text: None,
        int: None,
        int_str: None,
        bool: None,
        text_array: None,
    }
}

#[must_use]
pub fn cell_text(s: String) -> JsCell {
    JsCell {
        kind: "text".into(),
        text: Some(s),
        int: None,
        int_str: None,
        bool: None,
        text_array: None,
    }
}

#[must_use]
pub fn cell_int(n: i64) -> JsCell {
    JsCell {
        kind: "int".into(),
        text: None,
        int: None,
        int_str: Some(n.to_string()),
        bool: None,
        text_array: None,
    }
}

#[must_use]
pub fn cell_int_str(s: String) -> JsCell {
    JsCell {
        kind: "int".into(),
        text: None,
        int: None,
        int_str: Some(s),
        bool: None,
        text_array: None,
    }
}

#[must_use]
pub fn cell_bool(b: bool) -> JsCell {
    JsCell {
        kind: "bool".into(),
        text: None,
        int: None,
        int_str: None,
        bool: Some(b),
        text_array: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constructible `Bind` variant is pinned to the cell it produces.
    ///
    /// Removing a conversion arm sends that value through the fallback and makes
    /// these behavior assertions fail. This protects the database write from a
    /// conversion silently changing a supplied value into SQL NULL.
    #[test]
    fn bind_fold_covers_every_variant() {
        assert_eq!(bind_to_cell(&Bind::Null).unwrap().kind, "null");

        let b = bind_to_cell(&Bind::Bool(true)).unwrap();
        assert_eq!(b.kind, "bool");
        assert_eq!(b.bool, Some(true));

        assert_eq!(
            bind_to_cell(&Bind::Int(42)).unwrap().int_str.as_deref(),
            Some("42")
        );
        assert_eq!(
            bind_to_cell(&Bind::Decimal("12345678901234567890.1234567890".into()))
                .unwrap()
                .text
                .as_deref(),
            Some("12345678901234567890.1234567890")
        );
        assert_eq!(
            bind_to_cell(&Bind::Text("x".into())).unwrap().text.as_deref(),
            Some("x")
        );

        // `Inferred` - the variant the old test did not cover at all.
        assert_eq!(
            bind_to_cell(&Bind::Inferred(Some("2026-09-04T00:00:00Z".into())))
                .unwrap()
                .text
                .as_deref(),
            Some("2026-09-04T00:00:00Z")
        );
        assert_eq!(
            bind_to_cell(&Bind::Inferred(None)).unwrap().kind,
            "null",
            "Inferred(None) is a declared SQL NULL and must stay one"
        );

        // No variant carrying a value may fold to a null cell. This is the
        // property the silent fallback violated, stated directly.
        for bind in [
            Bind::Bool(false),
            Bind::Int(0),
            Bind::Decimal("0".into()),
            Bind::Text(String::new()),
            Bind::Inferred(Some(String::new())),
        ] {
            let cell = bind_to_cell(&bind).expect("known variant must fold");
            assert_ne!(
                cell.kind, "null",
                "{bind:?} folded to a NULL cell - a value the engine asked to be \
                 written would be written as SQL NULL"
            );
        }
    }

    #[test]
    fn value_fold_roundtrips_int_via_string_and_number() {
        assert_eq!(
            cell_to_value(&cell_int_str("9223372036854775807".into())).unwrap(),
            Value::Int(i64::MAX)
        );
        assert_eq!(cell_to_value(&cell_int(5)).unwrap(), Value::Int(5));
        // A JS-number int
        let mut c = cell_null();
        c.kind = "int".into();
        c.int = Some(7.0);
        c.int_str = None;
        assert_eq!(cell_to_value(&c).unwrap(), Value::Int(7));
    }

    #[test]
    fn row_fold_rejects_length_mismatch() {
        let row = JsRow {
            columns: vec!["a".into(), "b".into()],
            cells: vec![cell_text("x".into())],
        };
        assert!(row_to_seam(&row).is_err());
    }
}
