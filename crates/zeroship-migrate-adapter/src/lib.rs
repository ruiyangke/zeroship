//! `zeroship-migrate-adapter` — the monorepo's native-PG producer for the
//! published `zero-migrate` engine's driver seam.
//!
//! The standalone [`zero_migrate`] engine is runtime-free and driver-free: its
//! Postgres apply path (the generic [`PostgresBackend`](zero_migrate::PostgresBackend),
//! the `<D: SqlSession>` journal/drift/precondition/baseline free functions, and
//! the `<D: SqlSession>` executor) is generic over the driver-neutral
//! [`zero_migrate::driver::SqlSession`] seam. The engine ships NO native Rust
//! network Postgres driver — the production producer is the napi/Node `pg` host.
//!
//! This crate supplies the MONOREPO's producer: [`CompioPgSession`], a newtype
//! wrapping [`compio_postgres::Client`] that implements [`SqlSession`], mapping the
//! neutral [`Bind`]/[`Value`]/[`Row`]/[`DbError`] seam types onto the platform's
//! io_uring PG driver. This lets the engine's apply flow run over the platform's
//! native compio driver directly — zero tokio, no Node in the loop.
//!
//! The newtype exists to satisfy Rust's orphan rule: both
//! `compio_postgres::Client` and [`SqlSession`] are foreign to this crate, so a
//! bare `impl SqlSession for compio_postgres::Client` is disallowed. The newtype
//! is the clean, allowed carrier.
//!
//! The mapping is a near-mechanical port of the monorepo's in-tree
//! `crates/zeroship-migrate/src/apply/backend/postgres/session.rs` `PgSession`
//! impl (whose neutral `Seam*` types are the SAME shape as the standalone's
//! `driver::*` types, renamed): `batch_execute → batch`, `execute → exec`,
//! `execute_text_params → exec_text`, `query`/`query_one` unchanged; the
//! `SeamBind → Value`/`SeamRow`/`SeamError` decode paths carry over byte-for-byte
//! (same OID → cell classification, same text-param `exec_text` PG coercion
//! reason).

use compio_postgres::types::{Kind, ToSql, Type};
use compio_postgres::{Client, Error as PgError, Row as PgRow};
use zero_migrate::driver::{Bind, DbError, Row, SqlSession, Value};

/// Phase F Stage 4a — the platform-schema migrate path on the published engine
/// (author `db/migrations-ts/*.ts` via zeroship-runtime V8 → apply via
/// zero-migrate over [`CompioPgSession`]). Feature-gated so the base library
/// surface (Stage 1's adapter) stays V8-free.
#[cfg(feature = "platform-cli")]
pub mod platform;

/// A monorepo-native [`SqlSession`] over a pinned [`compio_postgres::Client`].
///
/// The engine drives ONE verb at a time over a single pinned session (temp tables /
/// open transactions created by one verb must be visible to the next), which the
/// single physical compio connection satisfies. Construct via [`CompioPgSession::connect`]
/// (opens + detaches the connection run-loop) or [`CompioPgSession::new`] (wrap an
/// already-connected client whose run-loop the caller drives).
#[derive(Debug)]
pub struct CompioPgSession {
    client: Client,
}

impl CompioPgSession {
    /// Wrap an already-connected [`compio_postgres::Client`] as a [`SqlSession`].
    ///
    /// The caller is responsible for driving the client's `Connection` run-loop
    /// (spawn + detach, or hold the handle). Prefer [`CompioPgSession::connect`]
    /// for the common case.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Open a fresh session to `dsn`, spawning + detaching its driver loop on the
    /// current compio runtime (the crate-wide `connect` + `spawn(conn.run()).detach()`
    /// pattern used across `crates/control`, `crates/auth`, and the in-tree
    /// `zeroship-migrate::conn::connect`).
    ///
    /// # Errors
    /// Returns the underlying [`compio_postgres::Error`] if the session cannot be
    /// established.
    pub async fn connect(dsn: &str) -> Result<Self, PgError> {
        let (client, connection) = compio_postgres::connect(dsn, compio_postgres::NoTls).await?;
        compio::runtime::spawn(async move {
            if let Err(e) = connection.run().await {
                tracing::error!(error = %e, "zeroship-migrate-adapter: pg connection loop ended with error");
            }
        })
        .detach();
        Ok(Self::new(client))
    }

    /// Borrow the underlying compio client (for out-of-band probes in tests /
    /// callers that need a raw verb outside the seam).
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }
}

// ---------------------------------------------------------------------------
// Neutral-type mapping — the FIRST monorepo producer of every `driver::*` type.
// Ported from `zeroship-migrate/src/apply/backend/postgres/session.rs`.
// ---------------------------------------------------------------------------

/// `compio_postgres::Error → driver::DbError`. The engine treats the error
/// opaquely (`Display`/`#[source]`), so `Display` + optional SQLSTATE is a
/// faithful, lossless-enough projection. The human-meaningful text lives on the
/// underlying `DbError` (the primary server message); project that into
/// `DbError.message` so the neutral error carries the same diagnostic rather than
/// degrading to the opaque "db error".
fn to_db_error(e: &PgError) -> DbError {
    let sqlstate = e.code().map(|c| c.code().to_string());
    let message = e
        .as_db_error()
        .map_or_else(|| e.to_string(), |db| db.message().to_string());
    DbError { message, sqlstate }
}

/// A borrowed `ToSql`-holder produced from a [`Bind`], so the compio `Client`'s
/// `&[&(dyn ToSql + Sync)]` slice can borrow into it for the duration of the call.
///
/// `Decimal` maps to a text bind (the IR carries decimals as strings; PG infers
/// the target type from context).
enum ToSqlHolder {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
}

impl ToSqlHolder {
    fn as_to_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            // A typed NULL: `Option::<&str>::None` renders as a text NULL bind.
            Self::Null => &Option::<&str>::None,
            Self::Bool(b) => b,
            Self::Int(n) => n,
            Self::Text(s) => s,
        }
    }
}

fn unsupported_bind_to_holder<T>(_: &T) -> Result<ToSqlHolder, DbError> {
    Err(DbError::message("unsupported bind variant"))
}

fn to_holder(bind: &Bind) -> Result<ToSqlHolder, DbError> {
    match bind {
        Bind::Null => Ok(ToSqlHolder::Null),
        Bind::Bool(b) => Ok(ToSqlHolder::Bool(*b)),
        Bind::Int(n) => Ok(ToSqlHolder::Int(*n)),
        // Decimal carried as text — PG infers the numeric target from context.
        Bind::Decimal(s) => Ok(ToSqlHolder::Text(s.clone())),
        Bind::Text(s) => Ok(ToSqlHolder::Text(s.clone())),
        _ => unsupported_bind_to_holder(bind),
    }
}

fn bind_holders(params: &[Bind]) -> Result<Vec<ToSqlHolder>, DbError> {
    params.iter().map(to_holder).collect()
}

fn holder_refs(holders: &[ToSqlHolder]) -> Vec<&(dyn ToSql + Sync)> {
    holders.iter().map(ToSqlHolder::as_to_sql).collect()
}

/// Resolve a `Kind::Domain(base)` chain down to its concrete base type, so an
/// `information_schema` domain (`cardinal_number` over `int4`, `sql_identifier`
/// over `name`, `yes_or_no` over `varchar`) routes to the right decode arm.
fn resolve_domain(ty: &Type) -> &Type {
    let mut cur = ty;
    while let Kind::Domain(inner) = cur.kind() {
        cur = inner;
    }
    cur
}

/// A text-family base type — decoded as `String` / `TextArray` element.
fn is_text_family(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::TEXT | Type::NAME | Type::VARCHAR | Type::BPCHAR | Type::UNKNOWN
    )
}

/// Decode ONE `compio_postgres::Row` cell into a neutral [`Value`], reproducing the
/// in-tree adapter's classification byte-for-byte: text-family / `"char"` /
/// `to_char`-timestamps → `Text`; int2/int4/int8/oid → `Int`; bool → `Bool`;
/// text[] → `TextArray` (element-NULLs preserved); SQL NULL → `Null`.
fn cell_to_value(row: &PgRow, idx: usize, ty: &Type) -> Result<Value, DbError> {
    let base = resolve_domain(ty);

    // Arrays first: `text[]` (and any `Kind::Array` whose element is text-family)
    // → `TextArray`. Also catches `information_schema` array domains.
    if let Kind::Array(elem) = base.kind()
        && is_text_family(resolve_domain(elem))
    {
        return match row.try_get::<_, Option<Vec<Option<String>>>>(idx) {
            Ok(Some(v)) => Ok(Value::TextArray(v)),
            Ok(None) => Ok(Value::Null),
            Err(e) => Err(DbError::message(format!("decode text[] cell: {e}"))),
        };
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
        // PG `"char"` (oid 18): decode the raw `i8` and normalise to a 1-char
        // `Text`, so `FromValue for i8`/`char` reads it back the same as native.
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

/// `compio_postgres::Row → driver::Row` — iterate columns, decode each cell.
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

/// The native, compio-postgres [`SqlSession`] impl — one forward per verb (mapping
/// binds/rows/errors through the neutral seam), so the SQL, txn boundaries, and
/// decoded domain values are identical to the platform's in-tree `PgSession` impl.
impl SqlSession for CompioPgSession {
    async fn batch(&self, sql: &str) -> Result<(), DbError> {
        self.client
            .batch_execute(sql)
            .await
            .map_err(|e| to_db_error(&e))
    }

    async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError> {
        let holders = bind_holders(binds)?;
        let refs = holder_refs(&holders);
        self.client
            .execute(sql, &refs)
            .await
            .map_err(|e| to_db_error(&e))
    }

    async fn exec_text(&self, sql: &str, params: &[Option<String>]) -> Result<u64, DbError> {
        // Text-format, server-inferred params (the load-bearing `text → timestamptz`
        // coercion the executor's lowered op.* DML relies on — a concrete-OID binary
        // bind would make PG refuse the coercion). `compio_postgres`'
        // `execute_text_params` already implements exactly this text-param path.
        self.client
            .execute_text_params(sql, params)
            .await
            .map_err(|e| to_db_error(&e))
    }

    async fn query(&self, sql: &str, binds: &[Bind]) -> Result<Vec<Row>, DbError> {
        let holders = bind_holders(binds)?;
        let refs = holder_refs(&holders);
        let rows = self
            .client
            .query(sql, &refs)
            .await
            .map_err(|e| to_db_error(&e))?;
        rows.iter().map(row_to_neutral).collect()
    }

    async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError> {
        let holders = bind_holders(binds)?;
        let refs = holder_refs(&holders);
        let row = self
            .client
            .query_one(sql, &refs)
            .await
            .map_err(|e| to_db_error(&e))?;
        row_to_neutral(&row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct UnrecognisedBind;

    #[test]
    fn unrecognised_bind_mapping_returns_error() {
        let result = unsupported_bind_to_holder(&UnrecognisedBind);
        let error = match result {
            Ok(_) => panic!("unrecognised bind mapped to a SQL holder"),
            Err(error) => error,
        };

        assert_eq!(error.message, "unsupported bind variant");
        assert!(error.sqlstate.is_none());
    }
}
