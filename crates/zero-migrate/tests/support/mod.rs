//! Shared test-support for the live-Postgres regression suite.
//!
//! [`PgDevSession`] is a TEST-ONLY [`zero_migrate::driver::SqlSession`] implementation
//! backed by the BLOCKING `postgres` crate. It lets the in-crate Rust tests drive the
//! SHIPPED generic PG apply path — `PostgresBackend<PgDevSession>`, the `<D: SqlSession>`
//! journal/drift/precondition/baseline free functions, `ops::status` — against a live
//! Postgres (:5440) through the SAME driver seam the production napi/Node `pg` host
//! rides. This is the in-crate live-DB coverage the deleted `native-pg` tests used to
//! provide (they drove the now-deleted compio client directly).
//!
//! **Never ships.** The `postgres` crate is a `[dev-dependency]` only. It pulls `tokio`
//! transitively (blocking `postgres` wraps `tokio-postgres` on a private current-thread
//! runtime), which is acceptable precisely because this driver is test-only — the
//! shipped `zero-migrate` library links neither tokio nor compio (`cargo tree -p
//! zero-migrate -e normal` stays empty of both).
//!
//! **Single pinned connection.** The seam contract is one verb at a time over
//! ONE pinned backend. The blocking `postgres::Client` is a single physical connection;
//! it is wrapped in a `RefCell` (the seam is `&self`, the client methods are `&mut self`,
//! and the runtime is single-threaded) so a temp table / open transaction created by one
//! verb is visible to the next — exactly what `apply_transactional` relies on.

#![allow(dead_code)] // Not every test binary uses every helper.

use std::cell::RefCell;

use postgres::types::{Format, IsNull, Kind, ToSql, Type};
use postgres::{Client, NoTls, Row as PgRow};
use bytes::BytesMut;

use zero_migrate::driver::{Bind, DbError, Row, SqlSession, Value};

/// The env var gating the live-Postgres tests. When unset, every live test skips
/// cleanly (so DB-free CI stays green); when set to a DSN, the suite runs against it.
pub const PG_URL_ENV: &str = "ZERO_MIGRATE_TEST_PG_URL";

/// Read the live-PG DSN from [`PG_URL_ENV`], or `None` when unset (→ skip).
///
/// Accepts either the libpq keyword form
/// (`host=… port=… user=… password=… dbname=…`) or a `postgres://…` URL — the
/// `postgres` crate parses both.
#[must_use]
pub fn pg_url() -> Option<String> {
    std::env::var(PG_URL_ENV).ok().filter(|s| !s.trim().is_empty())
}

/// Print the standard skip notice and return — used by every live test's early-out
/// when [`PG_URL_ENV`] is unset. Returns `true` when the caller should skip.
#[macro_export]
macro_rules! skip_if_no_pg {
    () => {{
        match $crate::support::pg_url() {
            Some(url) => url,
            None => {
                eprintln!(
                    "skipping live-PG test: {} unset (set it to a DSN on :5440 to run)",
                    $crate::support::PG_URL_ENV
                );
                return;
            }
        }
    }};
}

/// A TEST-ONLY [`SqlSession`] over a blocking `postgres::Client`, pinned to ONE
/// connection (the seam's single-connection contract).
pub struct PgDevSession {
    client: RefCell<Client>,
}

impl PgDevSession {
    /// Connect a fresh pinned session to `dsn`.
    ///
    /// # Panics
    /// Panics if the connection fails — a test-support harness, so a connect failure
    /// is a test setup error (the caller skips via [`skip_if_no_pg!`] when the DSN is
    /// simply absent).
    #[must_use]
    pub fn connect(dsn: &str) -> Self {
        let client = Client::connect(dsn, NoTls)
            .unwrap_or_else(|e| panic!("PgDevSession: connect to {dsn} failed: {e}"));
        Self {
            client: RefCell::new(client),
        }
    }
}

/// Map a `postgres::Error` → the neutral [`DbError`] (message + real SQLSTATE), so
/// the seam's error contract (`role.rs` transient-retry classifier, `#[source]`
/// wraps, the conformance `error-sqlstate-mapping` check) sees the true DB error.
fn to_db_error(e: &postgres::Error) -> DbError {
    // Prefer the structured DbError (server message) when present; fall back to the
    // Display form. Carry the real SQLSTATE so the conformance check can assert it.
    let sqlstate = e.code().map(|c| c.code().to_string());
    let message = e
        .as_db_error()
        .map_or_else(|| e.to_string(), |db| db.message().to_string());
    DbError {
        message,
        sqlstate,
    }
}

/// Resolve a `Kind::Domain(base)` chain down to its concrete base type — so an
/// `information_schema` domain (`cardinal_number` over int4, `sql_identifier` over
/// name, `yes_or_no` over varchar) routes to the right decode arm, exactly as the
/// deleted native compio adapter did.
fn resolve_domain(ty: &Type) -> &Type {
    let mut cur = ty;
    while let Kind::Domain(inner) = cur.kind() {
        cur = inner;
    }
    cur
}

/// A text-family base type — decoded as `String` / `TextArray` element.
const fn is_text_family(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::TEXT | Type::NAME | Type::VARCHAR | Type::BPCHAR | Type::UNKNOWN
    )
}

/// Decode ONE `postgres::Row` cell into a neutral [`Value`], reproducing the deleted
/// native adapter's classification byte-for-byte: text-family / `"char"` /
/// `to_char`-timestamps → `Text`; int2/int4/int8/oid → `Int`; numeric → `Decimal`;
/// bool → `Bool`; text[] → `TextArray` (element-NULLs preserved); SQL NULL → `Null`.
fn cell_to_value(row: &PgRow, idx: usize, ty: &Type) -> Result<Value, DbError> {
    let base = resolve_domain(ty);

    // Arrays first: a text-family element array → TextArray. Also catches
    // information_schema array domains.
    if let Kind::Array(elem) = base.kind() {
        if is_text_family(resolve_domain(elem)) {
            return match row.try_get::<_, Option<Vec<Option<String>>>>(idx) {
                Ok(Some(v)) => Ok(Value::TextArray(v)),
                Ok(None) => Ok(Value::Null),
                Err(e) => Err(DbError::message(format!("decode text[] cell: {e}"))),
            };
        }
    }

    match *base {
        Type::BOOL => match row.try_get::<_, Option<bool>>(idx) {
            Ok(Some(b)) => Ok(Value::Bool(b)),
            Ok(None) => Ok(Value::Null),
            Err(e) => Err(DbError::message(format!("decode bool: {e}"))),
        },
        Type::INT8 => match row.try_get::<_, Option<i64>>(idx) {
            Ok(Some(n)) => Ok(Value::Int(n)),
            Ok(None) => Ok(Value::Null),
            Err(e) => Err(DbError::message(format!("decode int8: {e}"))),
        },
        Type::INT4 | Type::OID => match row.try_get::<_, Option<i32>>(idx) {
            Ok(Some(n)) => Ok(Value::Int(i64::from(n))),
            Ok(None) => Ok(Value::Null),
            Err(e) => Err(DbError::message(format!("decode int4/oid: {e}"))),
        },
        Type::INT2 => match row.try_get::<_, Option<i16>>(idx) {
            Ok(Some(n)) => Ok(Value::Int(i64::from(n))),
            Ok(None) => Ok(Value::Null),
            Err(e) => Err(DbError::message(format!("decode int2: {e}"))),
        },
        // numeric/decimal → the canonical string form (no f64 in the IR identity).
        // tokio-postgres has no native Decimal decode without a feature, so read the
        // server's text form directly.
        Type::NUMERIC => match row.try_get::<_, Option<PgNumericText>>(idx) {
            Ok(Some(PgNumericText(s))) => Ok(Value::Decimal(s)),
            Ok(None) => Ok(Value::Null),
            Err(e) => Err(DbError::message(format!("decode numeric: {e}"))),
        },
        // PG `"char"` (oid 18): decode the raw i8 and normalise to a 1-char Text,
        // so `FromValue for i8`/`char` reads it back the same as native.
        Type::CHAR => match row.try_get::<_, Option<i8>>(idx) {
            Ok(Some(c)) => {
                let byte = u8::try_from(c).map_err(|_| {
                    DbError::message(format!("\"char\" byte {c} out of ASCII range"))
                })?;
                Ok(Value::Text((byte as char).to_string()))
            }
            Ok(None) => Ok(Value::Null),
            Err(e) => Err(DbError::message(format!("decode \"char\": {e}"))),
        },
        // text/name/varchar/bpchar and any other text-family type the SQL casts to.
        _ => match row.try_get::<_, Option<String>>(idx) {
            Ok(Some(s)) => Ok(Value::Text(s)),
            Ok(None) => Ok(Value::Null),
            Err(e) => Err(DbError::message(format!(
                "decode text-family cell (type {base}): {e}"
            ))),
        },
    }
}

/// A `FromSql` shim that reads a Postgres `numeric` as its server TEXT form — so a
/// decimal crosses the seam as its canonical string (parity with `Bind::Decimal` /
/// `Value::Decimal`), never lossy f64.
struct PgNumericText(String);

impl<'a> postgres::types::FromSql<'a> for PgNumericText {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        // Decode the numeric binary form via the standard string decoder path: the
        // simplest robust route is to read the value the server rendered. tokio-postgres
        // sends numeric in binary; parse it via the `postgres-protocol` numeric reader
        // is heavyweight — instead we rely on the SQL casting numeric columns to text
        // where exactness matters. For a raw numeric column, fall back to a decimal
        // string reconstruction.
        let s = pg_numeric_from_binary(raw)?;
        Ok(Self(s))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::NUMERIC
    }
}

/// Reconstruct a decimal string from Postgres' binary `numeric` wire form.
///
/// The binary numeric is: `i16 ndigits, i16 weight, u16 sign, u16 dscale`, then
/// `ndigits` base-10000 `i16` digit groups. This yields the exact decimal string
/// with no f64 rounding.
fn pg_numeric_from_binary(
    raw: &[u8],
) -> Result<String, Box<dyn std::error::Error + Sync + Send>> {
    if raw.len() < 8 {
        return Err("numeric binary too short".into());
    }
    let rd_i16 = |b: &[u8]| i16::from_be_bytes([b[0], b[1]]);
    let rd_u16 = |b: &[u8]| u16::from_be_bytes([b[0], b[1]]);
    let ndigits = rd_i16(&raw[0..2]);
    let weight = rd_i16(&raw[2..4]);
    let sign = rd_u16(&raw[4..6]);
    let dscale = rd_u16(&raw[6..8]) as usize;
    let mut digits = Vec::new();
    let mut off = 8;
    for _ in 0..ndigits {
        if off + 2 > raw.len() {
            return Err("numeric binary truncated".into());
        }
        digits.push(rd_u16(&raw[off..off + 2]));
        off += 2;
    }
    // NaN.
    if sign == 0xC000 {
        return Ok("NaN".to_string());
    }
    // Build the integer + fractional parts from base-10000 groups.
    let mut int_part = String::new();
    let mut frac_part = String::new();
    // Groups before the decimal point: indices 0..=weight.
    for i in 0..=weight {
        let group = if (i as usize) < digits.len() {
            digits[i as usize]
        } else {
            0
        };
        if int_part.is_empty() {
            int_part.push_str(&group.to_string());
        } else {
            int_part.push_str(&format!("{group:04}"));
        }
    }
    if int_part.is_empty() {
        int_part.push('0');
    }
    // Fractional groups: indices weight+1..
    let mut i = weight + 1;
    while (i as usize) < digits.len() {
        frac_part.push_str(&format!("{:04}", digits[i as usize]));
        i += 1;
    }
    // Respect dscale (trailing zero padding / truncation to declared scale).
    if frac_part.len() < dscale {
        frac_part.push_str(&"0".repeat(dscale - frac_part.len()));
    } else if frac_part.len() > dscale {
        frac_part.truncate(dscale);
    }
    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    out.push_str(&int_part);
    if dscale > 0 {
        out.push('.');
        out.push_str(&frac_part);
    }
    Ok(out)
}

/// `postgres::Row → driver::Row` — iterate columns, decode each cell.
fn row_to_neutral(row: &PgRow) -> Result<Row, DbError> {
    let cols = row.columns();
    let mut names = Vec::with_capacity(cols.len());
    let mut values = Vec::with_capacity(cols.len());
    for (idx, col) in cols.iter().enumerate() {
        names.push(col.name().to_string());
        values.push(cell_to_value(row, idx, col.type_())?);
    }
    Ok(Row::new(names, values))
}

/// A borrowed `ToSql`-holder produced from a [`Bind`], so the blocking client's
/// `&[&(dyn ToSql + Sync)]` slice can borrow into it for the duration of the call.
enum ToSqlHolder {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
}

impl ToSqlHolder {
    fn as_to_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            Self::Null => &Option::<&str>::None,
            Self::Bool(b) => b,
            Self::Int(n) => n,
            Self::Text(s) => s,
        }
    }
}

fn to_holder(bind: &Bind) -> ToSqlHolder {
    match bind {
        Bind::Null => ToSqlHolder::Null,
        Bind::Bool(b) => ToSqlHolder::Bool(*b),
        Bind::Int(n) => ToSqlHolder::Int(*n),
        // Decimal carried as text — PG infers the numeric target from context.
        Bind::Decimal(s) => ToSqlHolder::Text(s.clone()),
        Bind::Text(s) => ToSqlHolder::Text(s.clone()),
        // `Bind` is #[non_exhaustive]; a future variant maps to a text NULL rather
        // than panicking.
        _ => ToSqlHolder::Null,
    }
}

fn bind_holders(params: &[Bind]) -> Vec<ToSqlHolder> {
    params.iter().map(to_holder).collect()
}

/// A `ToSql` wrapper that sends its value in **text format** with no concrete OID
/// coercion — the `exec_text` server-inference path (`text → timestamptz`). `None`
/// is a SQL NULL. It `accepts` every type (the server, not the client, decides the
/// target) and reports [`Format::Text`], so tokio-postgres frames the param
/// text-format exactly as node-pg does.
#[derive(Debug)]
struct TextParam(Option<String>);

impl ToSql for TextParam {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match &self.0 {
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }

    fn accepts(_ty: &Type) -> bool {
        // The server infers the target type from context; accept anything.
        true
    }

    fn to_sql_checked(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        // `TextParam` accepts every type, so the checked adaptor just forwards.
        self.to_sql(ty, out)
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }
}

fn holder_refs(holders: &[ToSqlHolder]) -> Vec<&(dyn ToSql + Sync)> {
    holders.iter().map(ToSqlHolder::as_to_sql).collect()
}

impl SqlSession for PgDevSession {
    async fn batch(&self, sql: &str) -> Result<(), DbError> {
        self.client
            .borrow_mut()
            .batch_execute(sql)
            .map_err(|e| to_db_error(&e))
    }

    async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError> {
        let holders = bind_holders(binds);
        let refs = holder_refs(&holders);
        self.client
            .borrow_mut()
            .execute(sql, &refs)
            .map_err(|e| to_db_error(&e))
    }

    async fn exec_text(&self, sql: &str, params: &[Option<String>]) -> Result<u64, DbError> {
        // Server-inferred TEXT params (the load-bearing text→timestamptz coercion).
        // Each param is sent as a TEXT-format value with `Type::UNKNOWN`, so PG
        // infers the target column type from the statement context — exactly what
        // node-pg does (send text-format, no explicit OID) and what the shipped
        // `exec_text` contract requires. A concrete-OID binary bind would make PG
        // refuse `text → timestamptz`. `None` → SQL NULL. Drains the row iterator to
        // read `rows_affected`.
        let typed: Vec<(TextParam, Type)> = params
            .iter()
            .map(|p| (TextParam(p.clone()), Type::UNKNOWN))
            .collect();
        let mut client = self.client.borrow_mut();
        let iter = client
            .query_typed_raw(sql, typed)
            .map_err(|e| to_db_error(&e))?;
        // Drain to completion so `rows_affected` is populated (the statement ran).
        let (_rows, affected) = drain_row_iter(iter)?;
        Ok(affected)
    }

    async fn query(&self, sql: &str, binds: &[Bind]) -> Result<Vec<Row>, DbError> {
        let holders = bind_holders(binds);
        let refs = holder_refs(&holders);
        let rows = self
            .client
            .borrow_mut()
            .query(sql, &refs)
            .map_err(|e| to_db_error(&e))?;
        rows.iter().map(row_to_neutral).collect()
    }

    async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError> {
        let holders = bind_holders(binds);
        let refs = holder_refs(&holders);
        let row = self
            .client
            .borrow_mut()
            .query_one(sql, &refs)
            .map_err(|e| to_db_error(&e))?;
        row_to_neutral(&row)
    }
}

/// Drain a `RowIter` fully, returning (decoded rows, `rows_affected`). Used by
/// `exec_text` (ignores rows, reads the count).
fn drain_row_iter(
    mut iter: postgres::RowIter<'_>,
) -> Result<(Vec<Row>, u64), DbError> {
    use postgres::fallible_iterator::FallibleIterator;
    let mut out = Vec::new();
    loop {
        match iter.next() {
            Ok(Some(row)) => out.push(row_to_neutral(&row)?),
            Ok(None) => break,
            Err(e) => return Err(to_db_error(&e)),
        }
    }
    let affected = iter.rows_affected().unwrap_or(0);
    Ok((out, affected))
}
