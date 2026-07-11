//! Driver-neutral value/row/error/bind types for the [`PgSession`](super::PgSession)
//! seam (design §A).
//!
//! The `PgSession` trait's five verbs used to name `compio_postgres::{Row, Error,
//! types::ToSql}` on both the call side (bind params) and the return side (rows +
//! error). Those are `compio_postgres` types with private constructors, so a
//! non-compio (Node/napi) host driver could neither be *called* (it cannot extract
//! cells from `&[&dyn ToSql]`, which serialize to PG binary wire format) nor
//! *return* rows/errors (private ctors). This module supplies the neutral
//! intermediate representation both sides bottom out in:
//!
//! - [`SeamBind`] — a field-for-field mirror of [`render::step::BindValue`](crate::render::step::BindValue),
//!   so the IR-fold path and the `PgSession` bind path share ONE neutral shape.
//! - [`SeamValue`] — the neutral cell (`Null`/`Text`/`Int`/`Bool`/`TextArray`),
//!   text-biased to match the SQL the apply path already emits (every timestamp is
//!   `to_char`-cast to text, every count is `bigint`, most arrays are `array_agg`).
//! - [`SeamRow`] — a driver-neutral row with `len()`/`get`/`try_get` shaped
//!   identically to `compio_postgres::Row`, so the ~26 decode sites read
//!   unchanged, resolving through [`FromSeam`] instead of `FromSql`.
//! - [`SeamError`] — a neutral, opaque error (message + optional SQLSTATE), which
//!   is exactly how every seam consumer treats the error (wrap as `#[source]`).
//!
//! The compio (`native-pg`) adapter is the FIRST producer of all four (see
//! `session.rs`), so the platform's native path proves the widening before any
//! host driver exists. It is the *only* place still touching
//! `compio_postgres::{types::ToSql, types::FromSql, Row, Error}`.

use std::error::Error as StdError;
use std::fmt;

/// A typed scalar bound into a parameterized `execute`/`query`/`query_one` call.
///
/// Mirrors [`render::step::BindValue`](crate::render::step::BindValue) exactly so
/// the IR-fold path and the `PgSession` bind path share ONE neutral shape. Today
/// only `Text` and `Int` binds reach the wire (schema/version/checksum/actor/ident
/// idents + `exec_ms`, §A.1); the `Bool`/`Null`/`Decimal` arms are enum-complete
/// but dormant on the live suite (exercised by the completeness test).
#[derive(Debug, Clone, PartialEq)]
pub enum SeamBind {
    /// SQL `NULL`.
    Null,
    /// A boolean.
    Bool(bool),
    /// An exact 64-bit integer (`exec_ms`; the only integer domain the IR admits).
    Int(i64),
    /// A decimal carried as its canonical string form (no `f64` in the IR identity).
    Decimal(String),
    /// A UTF-8 text value (schema/version/checksum/actor/ident/cursor).
    Text(String),
}

impl From<&str> for SeamBind {
    fn from(s: &str) -> Self {
        SeamBind::Text(s.to_string())
    }
}

impl From<&&str> for SeamBind {
    fn from(s: &&str) -> Self {
        SeamBind::Text((*s).to_string())
    }
}

impl From<&String> for SeamBind {
    fn from(s: &String) -> Self {
        SeamBind::Text(s.clone())
    }
}

impl From<String> for SeamBind {
    fn from(s: String) -> Self {
        SeamBind::Text(s)
    }
}

impl From<i64> for SeamBind {
    fn from(n: i64) -> Self {
        SeamBind::Int(n)
    }
}

impl From<&i64> for SeamBind {
    fn from(n: &i64) -> Self {
        SeamBind::Int(*n)
    }
}

/// A nullable text bind (`Option<String>`): `None → NULL`, `Some(s) → Text` —
/// mirrors compio's `FromSql for Option<String>` NULL handling (`backfill.rs`'s
/// `last_cursor` progress write).
impl From<&Option<String>> for SeamBind {
    fn from(s: &Option<String>) -> Self {
        match s {
            Some(v) => SeamBind::Text(v.clone()),
            None => SeamBind::Null,
        }
    }
}

impl From<Option<String>> for SeamBind {
    fn from(s: Option<String>) -> Self {
        match s {
            Some(v) => SeamBind::Text(v),
            None => SeamBind::Null,
        }
    }
}

/// A driver-neutral decoded cell.
///
/// A closed enum covering exactly the value union the PG apply path reads (§A.2),
/// text-biased to match the SQL that already exists. `i8`/"char", `i32`
/// (`character_maximum_length`), and `to_char` timestamps all funnel into `Text`
/// or `Int`; there is no `bytea`/`numeric`/`uuid`/`json`/native-timestamp variant
/// because no consumer reads one through the seam.
#[derive(Debug, Clone, PartialEq)]
pub enum SeamValue {
    /// SQL `NULL`.
    Null,
    /// text/name/varchar/bpchar, `"char"`-as-1-char, `to_char` timestamps.
    Text(String),
    /// int2/int4/int8 all widened to `i64`.
    Int(i64),
    /// bool.
    Bool(bool),
    /// `text[]`; element NULLs preserved (a `None` element).
    TextArray(Vec<Option<String>>),
}

/// A driver-neutral error surfaced by a [`PgSession`](super::PgSession) verb.
///
/// Every seam consumer treats the error opaquely (wraps it as `#[source]`/`Display`);
/// the only structured DB-error inspection in the whole apply path is
/// `role.rs`'s `as_db_error().message().contains(...)`, which is on the concrete
/// admin `Client` OUTSIDE this seam (its sole caller is the shadow harness, `None`
/// on the generic path — §A.5). So a `message` + optional `code` (SQLSTATE) is
/// sufficient. `code` is carried but unread by any current seam consumer — it
/// exists so a future retry/branch has a home without another widening.
#[derive(Debug, Clone)]
pub struct SeamError {
    /// Human-readable message — satisfies all seam consumers (`Display`/`#[source]`).
    pub message: String,
    /// SQLSTATE if the driver surfaces one; no seam consumer reads it today.
    pub code: Option<String>,
}

impl SeamError {
    /// Construct a message-only error (no SQLSTATE).
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            code: None,
        }
    }
}

impl fmt::Display for SeamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for SeamError {}

/// A driver-neutral row: named + positional cell access, shaped identically to
/// `compio_postgres::Row` so the domain decode reads unchanged.
#[derive(Debug, Clone)]
pub struct SeamRow {
    columns: Vec<String>,
    values: Vec<SeamValue>,
}

impl SeamRow {
    /// Build a row from parallel column-name / cell-value vectors.
    ///
    /// # Panics
    /// Panics (debug) if the two vectors differ in length — a producer bug.
    #[must_use]
    pub fn new(columns: Vec<String>, values: Vec<SeamValue>) -> Self {
        debug_assert_eq!(
            columns.len(),
            values.len(),
            "SeamRow: columns/values length mismatch"
        );
        Self { columns, values }
    }

    /// Number of columns in the row (→ `precondition.rs:543`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// `true` if the row has no columns.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn cell(&self, idx: &impl SeamIndex) -> Option<&SeamValue> {
        idx.resolve(&self.columns).and_then(|i| self.values.get(i))
    }

    /// Decode a cell, panicking on error — mirrors `Row::get`.
    ///
    /// The two type params `<I, T>` mirror `compio_postgres::Row::get::<I, T>` so
    /// the existing turbofish call sites (`r.get::<_, String>("v")`) read unchanged.
    ///
    /// # Panics
    /// Panics if the column is missing or the cell cannot be decoded to `T`.
    #[track_caller]
    pub fn get<I: SeamIndex, T: FromSeam>(&self, idx: I) -> T {
        match self.try_get::<I, T>(idx) {
            Ok(v) => v,
            Err(e) => panic!("error retrieving column: {e}"),
        }
    }

    /// Decode a cell, returning `Result` — mirrors `Row::try_get`.
    pub fn try_get<I: SeamIndex, T: FromSeam>(&self, idx: I) -> Result<T, SeamError> {
        let Some(cell) = self.cell(&idx) else {
            return Err(SeamError::message("column not found in row"));
        };
        T::from_seam(cell)
    }
}

/// Name (`&str`) or position (`usize`) column indexing — both access patterns in
/// the census (name for `r.get("relkind")`, position for
/// `row.try_get::<_, bool>(0)` / `.get::<_, i64>(0)`).
pub trait SeamIndex {
    /// Resolve to a positional index within `columns`, or `None` if absent.
    fn resolve(&self, columns: &[String]) -> Option<usize>;
}

impl SeamIndex for usize {
    fn resolve(&self, columns: &[String]) -> Option<usize> {
        if *self < columns.len() {
            Some(*self)
        } else {
            None
        }
    }
}

impl SeamIndex for &str {
    fn resolve(&self, columns: &[String]) -> Option<usize> {
        columns.iter().position(|c| c == self)
    }
}

/// The neutral analogue of `compio_postgres::types::FromSql`, implemented for
/// exactly the closed set the PG apply path decodes (§A.2/§A.4). A small, closed
/// trait (~10 impls), not the open `FromSql` ecosystem.
pub trait FromSeam: Sized {
    /// Decode `self` from a neutral cell, or a [`SeamError`] on a type/NULL mismatch.
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError>;
}

fn type_err(want: &str, got: &SeamValue) -> SeamError {
    SeamError::message(format!("cannot decode {got:?} as {want}"))
}

impl FromSeam for String {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Text(s) => Ok(s.clone()),
            other => Err(type_err("String", other)),
        }
    }
}

impl FromSeam for Option<String> {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Null => Ok(None),
            SeamValue::Text(s) => Ok(Some(s.clone())),
            other => Err(type_err("Option<String>", other)),
        }
    }
}

impl FromSeam for bool {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Bool(b) => Ok(*b),
            other => Err(type_err("bool", other)),
        }
    }
}

impl FromSeam for i64 {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Int(n) => Ok(*n),
            other => Err(type_err("i64", other)),
        }
    }
}

impl FromSeam for Option<i64> {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Null => Ok(None),
            SeamValue::Int(n) => Ok(Some(*n)),
            other => Err(type_err("Option<i64>", other)),
        }
    }
}

impl FromSeam for i32 {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Int(n) => i32::try_from(*n)
                .map_err(|_| SeamError::message(format!("i32 out of range: {n}"))),
            other => Err(type_err("i32", other)),
        }
    }
}

impl FromSeam for Option<i32> {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Null => Ok(None),
            SeamValue::Int(n) => i32::try_from(*n)
                .map(Some)
                .map_err(|_| SeamError::message(format!("i32 out of range: {n}"))),
            other => Err(type_err("Option<i32>", other)),
        }
    }
}

/// PG `"char"` (oid 18): the four consumers read it as `i8` then
/// `u8::try_from(x).map(char::from)`. The adapter normalises `"char"` to a 1-char
/// `Text`, so `FromSeam for i8` reads the single byte back out.
impl FromSeam for i8 {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Text(s) => {
                let mut it = s.chars();
                match (it.next(), it.next()) {
                    (Some(c), None) if c.is_ascii() => Ok(c as i8),
                    _ => Err(SeamError::message(format!(
                        "cannot decode {s:?} as a 1-char \"char\" (i8)"
                    ))),
                }
            }
            other => Err(type_err("i8", other)),
        }
    }
}

impl FromSeam for char {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Text(s) => {
                let mut it = s.chars();
                match (it.next(), it.next()) {
                    (Some(c), None) => Ok(c),
                    _ => Err(SeamError::message(format!(
                        "cannot decode {s:?} as a single char"
                    ))),
                }
            }
            other => Err(type_err("char", other)),
        }
    }
}

/// `text[]` decoded to `Vec<String>`. Reproduces the LEGACY compio outcome on an
/// element-NULL: `compio_postgres`' `FromSql for Vec<String>` cannot hold a NULL
/// element (a non-nullable `String` element), so it **errors** — and every call
/// site coerces that error to `[]` via `.unwrap_or_default()`. This impl errors
/// the same way so `.unwrap_or_default()` yields `[]`, keeping native and host
/// byte-identical (§A.8 parity).
impl FromSeam for Vec<String> {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::TextArray(elems) => {
                if elems.iter().any(Option::is_none) {
                    return Err(SeamError::message(
                        "text[] contains a NULL element (cannot decode to Vec<String>)",
                    ));
                }
                Ok(elems.iter().map(|e| e.clone().unwrap_or_default()).collect())
            }
            other => Err(type_err("Vec<String>", other)),
        }
    }
}

/// Nullable `text[]` decoded to `Option<Vec<String>>` (e.g. `reloptions`). A SQL
/// NULL array → `None`; a present array → `Some(...)`. Reproduces the legacy
/// compio outcome on an element-NULL: the inner `Vec<String>` decode errors, so
/// the caller's `.ok().flatten()` yields `None` — byte-identical to native
/// (§A.8 parity).
impl FromSeam for Option<Vec<String>> {
    fn from_seam(cell: &SeamValue) -> Result<Self, SeamError> {
        match cell {
            SeamValue::Null => Ok(None),
            SeamValue::TextArray(_) => <Vec<String>>::from_seam(cell).map(Some),
            other => Err(type_err("Option<Vec<String>>", other)),
        }
    }
}
