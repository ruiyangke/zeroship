//! napi ⇄ `zeroship_migrate` Seam-type marshaling (design §B.2).
//!
//! The host driver (`pg`/`mysql2` in JS) speaks JS cells; the engine speaks the
//! driver-neutral [`SeamBind`]/[`SeamValue`]/[`SeamRow`]/[`SeamError`] types. This
//! module is the ONLY place those two representations meet.
//!
//! The `#[napi(object)]` structs below are the wire shape that crosses the N-API
//! boundary — plain owned data (`String`/`f64`/`Vec`/`bool`), all `Send + 'static`,
//! so they may ride a `ThreadsafeFunction` call and come back through a `done`
//! callback without ever carrying an engine `!Send` handle or a V8 handle (§B.3).
//!
//! Value union (§A.2, verified exhaustively): `Null | Text | Int | Bool | TextArray`.
//! Ints cross as JS strings when they exceed the safe-integer domain? — NO: the
//! engine's int domain is `i64`, but the ONLY ints the seam reads are small
//! catalog/count values (`character_maximum_length`, `relkind`-as-char, row
//! counts). We carry `Int` as an `f64` on the JS side (a JS `number`) for the
//! small values the seam actually reads, and additionally accept an `i64`-as-string
//! form (`intStr`) so a host `pg` type-parser that stringifies `int8`/`numeric`
//! (the §D.2 contract) round-trips exactly. `Text` covers the `to_char` timestamp
//! and all text/name/varchar cells.

use zeroship_migrate::apply::backend::postgres::seam::{SeamBind, SeamError, SeamRow, SeamValue};

#[cfg(feature = "napi")]
use napi_derive::napi;

/// The kind tag of a driver-neutral scalar cell — mirrors [`SeamValue`]'s variants.
///
/// A `#[napi(object)]` cannot be a Rust enum with data, so a cell crosses as a
/// tagged struct: `kind` selects the arm, and the matching payload field carries
/// the value (all others `None`/default). This keeps the union explicit on both
/// sides without a fragile positional encoding.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsCell {
    /// `"null" | "text" | "int" | "bool" | "textArray"`.
    pub kind: String,
    /// Payload for `kind == "text"`.
    pub text: Option<String>,
    /// Payload for `kind == "int"` when the value fits a JS `number` safely.
    pub int: Option<f64>,
    /// Payload for `kind == "int"` carried as a decimal string (int8/numeric that a
    /// `pg` type-parser stringified per §D.2). Preferred over `int` when present.
    pub int_str: Option<String>,
    /// Payload for `kind == "bool"`.
    pub bool: Option<bool>,
    /// Payload for `kind == "textArray"`; a JS `null` element is `None`.
    pub text_array: Option<Vec<Option<String>>>,
}

/// A driver-neutral row crossing the boundary: parallel column-name / cell vectors.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsRow {
    /// Column names, positionally aligned with `cells`.
    pub columns: Vec<String>,
    /// One [`JsCell`] per column.
    pub cells: Vec<JsCell>,
}

/// A successful verb reply crossing the boundary: the returned rows (empty for a
/// pure DML `execute`/`executeTextParams`) plus the driver's affected-/returned-row
/// count (`result.rowCount` from node-pg). The engine's `execute` verbs read the
/// count; `query`/`queryOne` read the rows.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsReply {
    /// Rows for `query`/`queryOne` (empty for pure DML).
    pub rows: Vec<JsRow>,
    /// `result.rowCount` — affected/returned rows. Carried as `f64` (JS `number`);
    /// `None` when the driver reports no count (e.g. `batch`).
    pub row_count: Option<f64>,
}

/// A driver-neutral error crossing the boundary (§A.5): message + optional SQLSTATE.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsError {
    /// Human-readable message (`err.message` from node-pg).
    pub message: String,
    /// SQLSTATE if the driver surfaced one (`err.code` from node-pg).
    pub code: Option<String>,
}

/// A single verb request the engine hands to the host driver (§B.2).
///
/// `kind` selects the verb (`batch | execute | executeTextParams | query |
/// queryOne`); `sql` is the statement; `binds` carries the neutral params for
/// `execute`/`query`/`queryOne`; `textParams` carries the `&[Option<String>]`
/// text-format params for `executeTextParams` (§B.2 — text-format, server-inferred
/// OID). Exactly one of `binds`/`textParams` is populated per verb kind.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsRequest {
    /// The verb: `"batch" | "execute" | "executeTextParams" | "query" | "queryOne"`.
    pub kind: String,
    /// The SQL statement.
    pub sql: String,
    /// Neutral binds for `execute`/`query`/`queryOne` (as [`JsCell`]s).
    pub binds: Vec<JsCell>,
    /// Text-format params for `executeTextParams` (`None` element → SQL NULL bind).
    pub text_params: Vec<Option<String>>,
}

// ---------------------------------------------------------------------------
// Conversions — the neutral ↔ JS-cell fold. Pure functions, no napi types, so
// this whole module (except the derive) compiles WITHOUT the `napi` feature and
// the mock-apply integration test exercises the folds directly.
// ---------------------------------------------------------------------------

/// `SeamBind → JsCell` (Rust → JS bind fold, §B.2). `Int→int`, `Text→text`,
/// `Bool→bool`, `Null→null`, `Decimal→text` (PG infers the numeric target from
/// context — the same fold the MySQL `bind_to_json` proves).
#[must_use]
pub fn bind_to_cell(bind: &SeamBind) -> JsCell {
    match bind {
        SeamBind::Null => cell_null(),
        SeamBind::Bool(b) => cell_bool(*b),
        // i64 binds only ever carry the small `exec_ms` today; cross as a string
        // so no precision is lost even if a future large bind appears.
        SeamBind::Int(n) => cell_int_str(n.to_string()),
        SeamBind::Decimal(s) => cell_text(s.clone()),
        SeamBind::Text(s) => cell_text(s.clone()),
    }
}

/// `JsCell → SeamValue` (JS → Rust return fold, §B.2). The `int`/`intStr` split:
/// `intStr` is preferred (exact int8/numeric), falling back to the `f64` `int`
/// narrowed to `i64`.
///
/// # Errors
/// Returns a message describing the malformed cell if `kind` is unknown or the
/// selected payload field is absent / unparseable.
pub fn cell_to_value(cell: &JsCell) -> Result<SeamValue, String> {
    match cell.kind.as_str() {
        "null" => Ok(SeamValue::Null),
        "text" => cell
            .text
            .clone()
            .map(SeamValue::Text)
            .ok_or_else(|| "text cell missing `text` payload".to_string()),
        "int" => {
            if let Some(s) = &cell.int_str {
                s.parse::<i64>()
                    .map(SeamValue::Int)
                    .map_err(|_| format!("int cell `intStr` not an i64: {s:?}"))
            } else if let Some(f) = cell.int {
                // A JS number carrying an integer; reject a non-integral or
                // out-of-range value rather than silently truncating.
                if f.fract() != 0.0 || f.abs() >= 9.007_199_254_740_992e15 {
                    Err(format!("int cell `int` not a safe integer: {f}"))
                } else {
                    Ok(SeamValue::Int(f as i64))
                }
            } else {
                Err("int cell missing both `int` and `intStr`".to_string())
            }
        }
        "bool" => cell
            .bool
            .map(SeamValue::Bool)
            .ok_or_else(|| "bool cell missing `bool` payload".to_string()),
        "textArray" => cell
            .text_array
            .clone()
            .map(SeamValue::TextArray)
            .ok_or_else(|| "textArray cell missing `textArray` payload".to_string()),
        other => Err(format!("unknown cell kind: {other:?}")),
    }
}

/// `JsRow → SeamRow`.
///
/// # Errors
/// Returns a message if any cell is malformed or columns/cells length-mismatch.
pub fn row_to_seam(row: &JsRow) -> Result<SeamRow, String> {
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
    Ok(SeamRow::new(row.columns.clone(), values))
}

/// `JsError → SeamError`.
#[must_use]
pub fn js_error_to_seam(e: &JsError) -> SeamError {
    SeamError {
        message: e.message.clone(),
        code: e.code.clone(),
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

    #[test]
    fn bind_fold_covers_every_variant() {
        assert_eq!(bind_to_cell(&SeamBind::Null).kind, "null");
        assert_eq!(bind_to_cell(&SeamBind::Bool(true)).kind, "bool");
        assert_eq!(
            bind_to_cell(&SeamBind::Int(42)).int_str.as_deref(),
            Some("42")
        );
        assert_eq!(
            bind_to_cell(&SeamBind::Decimal("1.5".into())).text.as_deref(),
            Some("1.5")
        );
        assert_eq!(
            bind_to_cell(&SeamBind::Text("x".into())).text.as_deref(),
            Some("x")
        );
    }

    #[test]
    fn value_fold_roundtrips_int_via_string_and_number() {
        assert_eq!(
            cell_to_value(&cell_int_str("9223372036854775807".into())).unwrap(),
            SeamValue::Int(i64::MAX)
        );
        assert_eq!(
            cell_to_value(&cell_int(5)).unwrap(),
            SeamValue::Int(5)
        );
        // A JS-number int
        let mut c = cell_null();
        c.kind = "int".into();
        c.int = Some(7.0);
        c.int_str = None;
        assert_eq!(cell_to_value(&c).unwrap(), SeamValue::Int(7));
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
