//! The dialect-neutral network driver seam (`SqlSession`).
//!
//! This is the ONE injected runtime dependency the network-dialect backends
//! (`PostgresBackend`, and the forthcoming `MysqlBackend`) are generic over.
//! **SQLite does NOT ride this seam** — it is an in-process `rusqlite` actor
//! (`apply::backend::sqlite`) with no session object. The seam is therefore an
//! implementation detail of the network backends, not a bound on the shared
//! [`MigrationBackend`](crate::apply::backend::MigrationBackend) trait.
//!
//! The trait is typed in DRIVER-NEUTRAL types ([`Bind`]/[`Value`]/[`Row`]/
//! [`DbError`]), NOT any concrete driver's row/error/bind types. Such driver types
//! typically have private constructors, so a host (napi) driver could neither be
//! *called* (it cannot extract cells from opaque bind params, which serialize to a
//! wire format) nor *return* rows/errors. The neutral types close both
//! sides:
//!
//! - [`Bind`] — a field-for-field mirror of [`render::step::BindValue`](crate::render::step::BindValue),
//!   so the IR-fold path and the seam bind path share ONE neutral shape.
//! - [`Value`] — the neutral decoded cell
//!   (`Null`/`Text`/`Int`/`Bool`/`Decimal`/`TextArray`), text-biased to match the
//!   SQL the apply path already emits (every timestamp is `to_char`-cast to text,
//!   every count is `bigint`, most arrays are `array_agg`).
//! - [`Row`] — a driver-neutral row with `len()`/`try_get` (NO panicking `get`),
//!   so the decode sites resolve through [`FromValue`] instead of `FromSql`.
//! - [`DbError`] — a neutral, opaque error (message + optional SQLSTATE), which
//!   is exactly how every seam consumer treats the error (wrap as `#[source]`).
//!
//! Both [`Bind`] and [`Value`] carry a `Decimal(String)` variant so they cover the
//! CLOSED IR value universe (`IrScalar` includes `Decimal`); both are
//! `#[non_exhaustive]` so a driver author matches with a wildcard arm.
//!
//! The napi host (`zero-migrate-node`'s `NapiHostSession`) is the production
//! producer of all four; the in-crate `RecordingSession` (a host-shaped,
//! in-process driver) proves the generic PG apply path is genuinely
//! driver-neutral before any dialect-specific SQL crosses the seam.

/// The driver-neutral [`SqlSession`] conformance suite — the FIRST external
/// consumer of the seam. A host driver (or the in-crate `PgDevSession`) runs
/// [`conformance::run`] against a live session to prove it honours the seam
/// invariants (session pinning, transaction visibility, `exec_text` semantics,
/// error/SQLSTATE mapping) the engine relies on but never re-checks.
#[path = "driver/conformance.rs"]
pub mod conformance;

use std::error::Error as StdError;
use std::fmt;

/// The in-session network driver surface the migrate apply path is generic over.
///
/// Exactly the in-session verbs the engine issues on a live session: `batch`
/// (DDL / txn control / multi-statement session setup), `exec` /
/// [`exec_text`](SqlSession::exec_text) (parameterized DML → rows affected), and
/// `query` / `query_one` (catalog / journal introspection → rows).
///
/// Every verb's error is the neutral [`DbError`] (uniform across all verbs). The
/// bind params are neutral [`Bind`] on `exec`/`query`/`query_one`; `exec_text`
/// keeps its already-neutral `&[Option<String>]` shape; `batch` takes no params.
///
/// The seam carries no transaction-object or lock abstraction: transaction
/// control (`BEGIN`/`COMMIT`/`ROLLBACK`), advisory locks, and confinement `SET`s
/// are SQL strings issued through `batch`/`exec` by each dialect's `Backend` — they
/// are engine logic, not driver methods.
#[allow(async_fn_in_trait)] // !Send is by design: the napi block_on worker + JS host are single-threaded
pub trait SqlSession {
    /// DDL / txn control / multi-statement session setup. Simple-query protocol:
    /// one `&str`, may contain `;`-separated statements, no params, no rows.
    async fn batch(&self, sql: &str) -> Result<(), DbError>;

    /// Parameterized DML → rows affected.
    async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError>;

    /// Schema-blind DML: **all params as server-inferred text**.
    ///
    /// Load-bearing and deliberately distinct from [`exec`](SqlSession::exec): the
    /// executor runs lowered op.* DML through this to dodge PG's concrete-OID
    /// binary-bind refusal of `text → timestamptz` (a concrete-OID binary bind
    /// makes Postgres refuse the coercion; the assembler needs text-format
    /// inference). Its BIND side is already neutral (`&[Option<String>]`); its
    /// ERROR side widens to [`DbError`] uniformly with the other verbs. A MySQL
    /// (`mysql2`) driver implements this as an alias for [`exec`](SqlSession::exec)
    /// (MySQL has no equivalent OID refusal). **Do not remove.**
    async fn exec_text(&self, sql: &str, params: &[Option<String>]) -> Result<u64, DbError>;

    /// Parameterized SELECT → all rows.
    async fn query(&self, sql: &str, binds: &[Bind]) -> Result<Vec<Row>, DbError>;

    /// Parameterized SELECT → exactly one row (errors otherwise).
    async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError>;
}

/// A typed scalar bound into a parameterized `exec`/`query`/`query_one` call.
///
/// Mirrors [`render::step::BindValue`](crate::render::step::BindValue) exactly so
/// the IR-fold path and the seam bind path share ONE neutral shape.
/// `#[non_exhaustive]`: the enum covers the closed IR value universe; a driver
/// author matches with a wildcard arm.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Bind {
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

impl From<&str> for Bind {
    fn from(s: &str) -> Self {
        Bind::Text(s.to_string())
    }
}

impl From<&&str> for Bind {
    fn from(s: &&str) -> Self {
        Bind::Text((*s).to_string())
    }
}

impl From<&String> for Bind {
    fn from(s: &String) -> Self {
        Bind::Text(s.clone())
    }
}

impl From<String> for Bind {
    fn from(s: String) -> Self {
        Bind::Text(s)
    }
}

impl From<i64> for Bind {
    fn from(n: i64) -> Self {
        Bind::Int(n)
    }
}

impl From<&i64> for Bind {
    fn from(n: &i64) -> Self {
        Bind::Int(*n)
    }
}

/// A nullable text bind (`Option<String>`): `None → NULL`, `Some(s) → Text` —
/// mirrors standard `FromSql for Option<String>` NULL handling (`backfill.rs`'s
/// `last_cursor` progress write).
impl From<&Option<String>> for Bind {
    fn from(s: &Option<String>) -> Self {
        match s {
            Some(v) => Bind::Text(v.clone()),
            None => Bind::Null,
        }
    }
}

impl From<Option<String>> for Bind {
    fn from(s: Option<String>) -> Self {
        match s {
            Some(v) => Bind::Text(v),
            None => Bind::Null,
        }
    }
}

/// A driver-neutral decoded cell.
///
/// A closed enum covering the value union the apply path reads, text-biased to
/// match the SQL that already exists. `i8`/"char", `i32`
/// (`character_maximum_length`), and `to_char` timestamps all funnel into `Text`
/// or `Int`. `Decimal` carries a canonical string (parity with [`Bind::Decimal`]
/// — both cover the closed IR value universe). `#[non_exhaustive]`: a driver
/// author matches with a wildcard arm.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// text/name/varchar/bpchar, `"char"`-as-1-char, `to_char` timestamps.
    Text(String),
    /// int2/int4/int8 all widened to `i64`.
    Int(i64),
    /// bool.
    Bool(bool),
    /// A decimal/numeric carried as its canonical string form (no `f64`).
    Decimal(String),
    /// `text[]`; element NULLs preserved (a `None` element).
    TextArray(Vec<Option<String>>),
}

/// A driver-neutral error surfaced by a [`SqlSession`] verb.
///
/// Every seam consumer treats the error opaquely (wraps it as `#[source]`/`Display`).
/// A `message` + optional `sqlstate` is sufficient; `sqlstate` is carried so a
/// future retry/branch has a home without another widening.
#[derive(Debug, Clone)]
pub struct DbError {
    /// Human-readable message — satisfies all seam consumers (`Display`/`#[source]`).
    pub message: String,
    /// SQLSTATE if the driver surfaces one; no seam consumer reads it today.
    pub sqlstate: Option<String>,
}

impl DbError {
    /// Construct a message-only error (no SQLSTATE).
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            sqlstate: None,
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for DbError {}

/// A driver-neutral row: named + positional cell access. Non-panicking: only
/// `try_get` is exposed (there is NO panicking `get`).
#[derive(Debug, Clone)]
pub struct Row {
    columns: Vec<String>,
    values: Vec<Value>,
}

impl Row {
    /// Build a row from parallel column-name / cell-value vectors.
    ///
    /// # Panics
    /// Panics (debug) if the two vectors differ in length — a producer bug.
    #[must_use]
    pub fn new(columns: Vec<String>, values: Vec<Value>) -> Self {
        debug_assert_eq!(
            columns.len(),
            values.len(),
            "driver::Row: columns/values length mismatch"
        );
        Self { columns, values }
    }

    /// Number of columns in the row.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// `true` if the row has no columns.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn cell(&self, idx: &impl ColIndex) -> Option<&Value> {
        idx.resolve(&self.columns).and_then(|i| self.values.get(i))
    }

    /// Decode a cell, returning `Result`.
    ///
    /// The two type params `<I, T>` let the existing call sites
    /// (`r.try_get::<_, String>("v")`) read unchanged. There is deliberately NO
    /// panicking `get` counterpart — a decode failure is always a `Result`.
    pub fn try_get<I: ColIndex, T: FromValue>(&self, idx: I) -> Result<T, DbError> {
        let Some(cell) = self.cell(&idx) else {
            return Err(DbError::message("column not found in row"));
        };
        T::from_value(cell)
    }
}

/// Name (`&str`) or position (`usize`) column indexing — both access patterns in
/// the census (name for `r.try_get("relkind")`, position for
/// `row.try_get::<_, bool>(0)`).
pub trait ColIndex {
    /// Resolve to a positional index within `columns`, or `None` if absent.
    fn resolve(&self, columns: &[String]) -> Option<usize>;
}

impl ColIndex for usize {
    fn resolve(&self, columns: &[String]) -> Option<usize> {
        if *self < columns.len() {
            Some(*self)
        } else {
            None
        }
    }
}

impl ColIndex for &str {
    fn resolve(&self, columns: &[String]) -> Option<usize> {
        columns.iter().position(|c| c == self)
    }
}

/// The neutral analogue of a driver's `FromSql`, implemented for
/// exactly the closed set the apply path decodes. A small, closed trait, not the
/// open `FromSql` ecosystem.
pub trait FromValue: Sized {
    /// Decode `self` from a neutral cell, or a [`DbError`] on a type/NULL mismatch.
    fn from_value(cell: &Value) -> Result<Self, DbError>;
}

fn type_err(want: &str, got: &Value) -> DbError {
    DbError::message(format!("cannot decode {got:?} as {want}"))
}

impl FromValue for String {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Text(s) => Ok(s.clone()),
            other => Err(type_err("String", other)),
        }
    }
}

impl FromValue for Option<String> {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Null => Ok(None),
            Value::Text(s) => Ok(Some(s.clone())),
            other => Err(type_err("Option<String>", other)),
        }
    }
}

impl FromValue for bool {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Bool(b) => Ok(*b),
            other => Err(type_err("bool", other)),
        }
    }
}

impl FromValue for i64 {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Int(n) => Ok(*n),
            other => Err(type_err("i64", other)),
        }
    }
}

impl FromValue for Option<i64> {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Null => Ok(None),
            Value::Int(n) => Ok(Some(*n)),
            other => Err(type_err("Option<i64>", other)),
        }
    }
}

impl FromValue for i32 {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Int(n) => {
                i32::try_from(*n).map_err(|_| DbError::message(format!("i32 out of range: {n}")))
            }
            other => Err(type_err("i32", other)),
        }
    }
}

impl FromValue for Option<i32> {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Null => Ok(None),
            Value::Int(n) => i32::try_from(*n)
                .map(Some)
                .map_err(|_| DbError::message(format!("i32 out of range: {n}"))),
            other => Err(type_err("Option<i32>", other)),
        }
    }
}

/// PG `"char"` (oid 18): the consumers read it as `i8` then
/// `u8::try_from(x).map(char::from)`. The adapter normalises `"char"` to a 1-char
/// `Text`, so `FromValue for i8` reads the single byte back out.
impl FromValue for i8 {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Text(s) => {
                let mut it = s.chars();
                match (it.next(), it.next()) {
                    (Some(c), None) if c.is_ascii() => Ok(c as i8),
                    _ => Err(DbError::message(format!(
                        "cannot decode {s:?} as a 1-char \"char\" (i8)"
                    ))),
                }
            }
            other => Err(type_err("i8", other)),
        }
    }
}

impl FromValue for char {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Text(s) => {
                let mut it = s.chars();
                match (it.next(), it.next()) {
                    (Some(c), None) => Ok(c),
                    _ => Err(DbError::message(format!(
                        "cannot decode {s:?} as a single char"
                    ))),
                }
            }
            other => Err(type_err("char", other)),
        }
    }
}

/// `text[]` decoded to `Vec<String>`. Reproduces the standard `FromSql`
/// outcome on an element-NULL: `FromSql for Vec<String>` cannot hold a NULL
/// element (a non-nullable `String` element), so it **errors** — and every call
/// site coerces that error to `[]` via `.unwrap_or_default()`. This impl errors
/// the same way so `.unwrap_or_default()` yields `[]`, keeping every driver's
/// output byte-identical.
impl FromValue for Vec<String> {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::TextArray(elems) => {
                if elems.iter().any(Option::is_none) {
                    return Err(DbError::message(
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
/// NULL array → `None`; a present array → `Some(...)`. Reproduces the standard
/// `FromSql` outcome on an element-NULL: the inner `Vec<String>` decode errors, so
/// the caller's `.ok().flatten()` yields `None` — byte-identical across drivers.
impl FromValue for Option<Vec<String>> {
    fn from_value(cell: &Value) -> Result<Self, DbError> {
        match cell {
            Value::Null => Ok(None),
            Value::TextArray(_) => <Vec<String>>::from_value(cell).map(Some),
            other => Err(type_err("Option<Vec<String>>", other)),
        }
    }
}

#[cfg(test)]
mod element_null_parity {
    //! Decode parity: a synthetic element-NULL `reloptions`-shaped `text[]`
    //! yields the `[]` outcome through a canned host [`Row`]. Pins parity on
    //! the one raw, nullable, unfiltered catalog array cell so any driver adapter
    //! stays interchangeable.

    use super::{Row, Value};

    /// A canned host row carrying a `text[]` with an element-NULL, exactly as a
    /// napi `pg` driver would build it (JS `null` inside the array).
    fn host_row_with_element_null() -> Row {
        Row::new(
            vec!["reloptions".to_string()],
            vec![Value::TextArray(vec![
                Some("fillfactor=90".to_string()),
                None, // a SQL NULL element
            ])],
        )
    }

    #[test]
    fn host_element_null_reloptions_coerces_to_empty_via_unwrap_or_default() {
        let row = host_row_with_element_null();
        let cols: Vec<String> = row.try_get::<_, Vec<String>>("reloptions").unwrap_or_default();
        assert!(
            cols.is_empty(),
            "element-NULL text[] must coerce to [] on host adapter"
        );
    }

    #[test]
    fn host_element_null_reloptions_coerces_to_none_via_ok_flatten() {
        let row = host_row_with_element_null();
        let opt: Option<Vec<String>> = row
            .try_get::<_, Option<Vec<String>>>("reloptions")
            .ok()
            .flatten();
        assert!(
            opt.is_none(),
            "element-NULL text[] must coerce to None on host adapter"
        );
    }
}
