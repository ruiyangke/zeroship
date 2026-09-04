//! `CompioPgSession` - this service's native-PG producer for the published
//! `zero-migrate` engine's driver seam.
//!
//! The standalone [`zeroship_migrate`] engine is runtime-free and driver-free: its
//! Postgres apply path (the generic
//! [`PostgresBackend`](zeroship_migrate_postgres::PostgresBackend), the
//! `<D: SqlSession>` journal/drift/precondition/baseline free functions, and the
//! `<D: SqlSession>` executor) is generic over the driver-neutral
//! [`zeroship_migrate::driver::SqlSession`] seam. The engine ships NO native Rust
//! network Postgres driver - the production producer is the napi/Node `pg` host.
//!
//! This module supplies the MONOREPO's producer: [`CompioPgSession`], a newtype
//! wrapping [`compio_postgres::Client`] that implements [`SqlSession`], mapping the
//! neutral [`Bind`]/[`Value`]/[`Row`]/[`DbError`] seam types onto the platform's
//! io_uring PG driver. This lets the engine's apply flow run over the platform's
//! native compio driver directly - zero tokio, no Node in the loop.
//!
//! The newtype exists to satisfy Rust's orphan rule: both
//! `compio_postgres::Client` and [`SqlSession`] are foreign to this crate, so a
//! bare `impl SqlSession for compio_postgres::Client` is disallowed. The newtype
//! is the clean, allowed carrier. THE ORPHAN RULE IS SATISFIED BY THE NEWTYPE
//! BEING LOCAL, NOT BY IT LIVING IN A CRATE OF ITS OWN, which is why this is a
//! module of the one crate that drives it rather than a crate of its own.
//!
//! # Lower IR through the GUARDED door only
//!
//! The engine exposes five lowering entry points and they are not equivalent.
//! Use `IrAuthor::load_and_lower_guarded` (or `load_and_lower`), which routes
//! through `model::load::load_ir_document_authorized`. THIS MODULE LOWERS
//! NOTHING - it is the session newtype and nothing else - so the consumers to
//! read are this crate's own `apply::apply_one_ir_file_postgres` and
//! `apply::preflight_ir_documents`, the two `load_and_lower_guarded` call sites.
//! The note stays because the seam this module exposes is what those callers
//! drive. They are named rather than cited by line number, which cannot rot.
//!
//! `IrAuthor::lower`, `lower_plan` and `lower_steps` take an ALREADY-deserialized
//! `MigrationIr` and do NOT run the loader. What that actually costs is narrower
//! than it first looks, and the difference is worth stating precisely because the
//! wrong version of it is alarming in a way that invites bad decisions.
//!
//! **Still enforced on the ungated path** (verified by reading the engine, not by
//! searching for symbol names):
//!
//! - SCHEMA CONFINEMENT. `lower` refuses a cross-schema op itself, on both arms -
//!   the inherited-default-schema case and the explicitly-qualified case - plus a
//!   fail-closed refusal on the SQLite leg. The engine comment beside it says this
//!   is deliberate: "Make `lower()` self-defending regardless of whether validate
//!   ran." It has its own tests.
//! - Six structural validations: authored identifier lengths, per-row DML
//!   destinations, typed column references, table foreign-key targets, typed
//!   reference catalogs, and the SQLite repeat-rename refusal. All three ungated
//!   doors run them, because `lower` and `lower_plan` both delegate to
//!   `lower_steps`.
//! - VENDOR CAPABILITY authority. `lower_steps` -> `lower_op_into_steps` ->
//!   `lower_one_op` -> `enforce_vendor_capability_at_lower`, which asks
//!   `policy_grants_capability` against the `EffectivePolicy` the `IrAuthor`
//!   already holds. Chain walked edge by edge, not inferred from adjacency.
//!
//!   Worth knowing why this one is easy to miss: the lowering copy shares no
//!   name with the loader's. The loader routes the same question through a value
//!   called `vendor_authority`; lowering spells it
//!   `enforce_vendor_capability_at_lower` + `policy_grants_capability`. A search
//!   for "vendor" finds the loader's accessor and misses the enforcement - a
//!   plausible non-zero result pointing at the wrong thing, which is worse than
//!   an empty one.
//!
//! **Believed loader-only.** These need the deploying app, the project registry
//! and the vendor authority that only the loader's CALLER holds, so a lower-side
//! equivalent is implausible rather than merely unfound:
//!
//! - `check_ir_version` (IR version, fail-closed)
//! - `enforce_ir_ownership` against the deploying app and project registry
//! - checksum-hint verification
//! - the `owner_app` stamp, which OVERWRITES whatever the artifact claimed
//!
//! The stamp is the one that matters most for us: it is what makes a spoofed
//! `owner_app` in a creator-supplied artifact inert. Deserializing an artifact
//! yourself and handing it to an ungated door carries the claimed value instead.
//!
//! ESTABLISHED AS LOAD-ONLY, which is different from unknown: the per-`Expr`
//! DIALECT-STRUCTURAL checks. The load walker validates every embedded
//! expression node against the target dialect, and that half does not re-run at
//! lower.
//!
//! But "embedded-expression rejection" is NOT one gate, and splitting it moves
//! half to the enforced column. The walker's own doc says its `ColRef`
//! RESOLUTION check runs at load only for a self-contained `createTable`, and
//! otherwise at the apply/render seam - which is
//! `validate_column_references_for_lower`, already listed above as one of the six
//! the ungated doors run. So the same gate appears on both of these lists under
//! two names, and reading either list alone gives the wrong answer.
//!
//! NOT ESTABLISHED EITHER WAY, and deliberately not asserted: invalid schema
//! ident, and illegal guard direction. Confinement, vendor authority and half of
//! the expression gate all turned out to be re-enforced, so the prior is that
//! these are too - but a prior is not a measurement, and absence must not be
//! inferred for them without finding the behaviour rather than the symbol.
//!
//! Deserializing IR directly is fine as a PRE-PASS - this crate
//! deserializes to resolve table-shape policy and re-serializes, then hands the
//! resolved bytes to the guarded door, which is the same shape the engine's own
//! Node addon uses. What must not happen is deserialize-then-lower.
//!
//! Established 2026-08-10 with the engine maintainers (inter-project thread
//! ZEROSHIP-2026-08-10-148 through ZERO-MIGRATE-2026-08-10-151). Recorded here
//! because nothing in the type system prevents reaching for the cheaper door:
//! the bypass is currently avoided by convention, not by construction.
//!
//! # What this module maps
//!
//! The seam is `zeroship_migrate_backend::driver::SqlSession`, re-exported as
//! [`zeroship_migrate::driver`]. It declares four async verbs - `batch`, `exec`,
//! `query`, `query_one` - over three neutral types, [`Bind`] in, [`Row`] of
//! [`Value`] cells out, [`DbError`] on failure. This module forwards each verb
//! onto [`compio_postgres::Client`]: `batch` -> `batch_execute`, `exec` ->
//! `execute_typed`, `query` / `query_one` -> `query_typed`, with
//! `bind_holders` / `holder_params` on the way in and `row_to_neutral` /
//! `cell_to_value` (OID -> cell classification) on the way out.
//!
//! THIS BLOCK CALLED THAT "a near-mechanical port of the engine's own
//! `crates/zeroship-migrate-postgres/src/backend/session.rs` `PgSession` impl"
//! UNTIL 2026-09-04, with a `SeamBind` / `SeamRow` / `SeamError` rename table.
//! None of those four names has ever existed anywhere in this tree - each
//! occurred only in that sentence, which a plain grep could not see because
//! `CompioPgSession` contains the substring `PgSession`. The cited file is real
//! and is the wrong artifact: it holds no `impl SqlSession for` anything, only
//! generic `<D: SqlSession>` free functions (project locking, session
//! snapshot/restore, transactional and non-transactional apply), so it CONSUMES
//! this seam rather than producing it. Nor was a `PgSession` ever renamed away:
//! `docs/proposals/2026-07-10-migrate-pg-driver-seam-design.md` opens by calling
//! the seam dialect-neutral, "`SqlSession`, not a PG-only `PgSession`".
//!
//! There was a port, and its source is simply not citable from here: the
//! standalone, out-of-repo `zero-migrate` project's own PG session. The engine
//! was in-sourced as `crates/zeroship-migrate*` and that source did not come
//! with it.
//!
//! The seam's `execute_text_params -> exec_text` leg is GONE, and its replacement is
//! not a rename. Server-inferred typing moved from a whole-statement verb to a
//! per-value one, [`Bind::Inferred`], so a statement can mix an inferred instant
//! with a typed key. This adapter carries it by binding through `execute_typed` /
//! `query_typed` with `Type::UNKNOWN` for those values - see `Untyped`.

use compio_postgres::types::{to_sql_checked, Format, IsNull, Kind, ToSql, Type};
use compio_postgres::types::private::BytesMut;
use compio_postgres::{Client, Error as PgError, Row as PgRow};
use zeroship_migrate::driver::{Bind, DbError, Row, SqlSession, Value};

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
                tracing::error!(error = %e, "zeroship-migrate-server: pg connection loop ended with error");
            }
        })
        .detach();
        Ok(Self::new(client))
    }

    /// Open a fresh session from an already-parsed [`compio_postgres::Config`],
    /// spawning + detaching its driver loop exactly as [`CompioPgSession::connect`]
    /// does.
    ///
    /// This exists so a caller can derive one DSN from another by editing a
    /// PARSED field rather than by string surgery on the URL. The platform
    /// cluster lock needs the caller's DSN with only `dbname` swapped, and a
    /// Postgres DSN admits percent-encoded userinfo, `?`-query parameters and
    /// comma-separated multi-host lists, so rewriting the database name with a
    /// regex or a `rsplit('/')` would silently corrupt a password containing `/`
    /// or `?`. Round-tripping through `Config` cannot.
    ///
    /// # Errors
    /// Returns the underlying [`compio_postgres::Error`] if the session cannot be
    /// established.
    pub async fn connect_with_config(config: &compio_postgres::Config) -> Result<Self, PgError> {
        let (client, connection) = config.connect(compio_postgres::NoTls).await?;
        compio::runtime::spawn(async move {
            if let Err(e) = connection.run().await {
                tracing::error!(error = %e, "zeroship-migrate-server: pg connection loop ended with error");
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
// Neutral-type mapping - the FIRST monorepo producer of every `driver::*` type.
// Ported from
// `crates/zeroship-migrate-postgres/src/backend/session.rs`.
// ---------------------------------------------------------------------------

/// `compio_postgres::Error -> driver::DbError`. The engine treats the error
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
    /// [`Bind::Inferred`]: text bytes with NO declared type, so the server reads
    /// the parameter's type off the column it lands in. See [`Untyped`].
    Untyped(Untyped),
}

/// A parameter the SERVER types, carried as text with its declared type left as
/// `UNKNOWN`.
///
/// This exists because PostgreSQL will not assign a parameter DECLARED as `text`
/// into a `timestamptz` column - there is no automatic cast - yet it accepts the
/// identical bytes when nothing is declared, because it then infers the type from
/// the target column and parses with that type's input function. The engine renders
/// DML from the IR, where an instant is a string and the column's type is unknown to
/// it, so "declare nothing" is the only correct choice for those values. That is
/// exactly what [`Bind::Inferred`] means, and its doc names `Type::UNKNOWN` as the
/// spelling a type-declaring driver must use.
///
/// The three overrides are each load-bearing: `accepts` admits any type because the
/// point is to make no claim about it; `encode_format` pins TEXT because the bytes
/// are the value's text form, not a binary encoding; `to_sql` writes them verbatim.
#[derive(Debug)]
struct Untyped(Option<String>);

impl ToSql for Untyped {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match &self.0 {
            None => Ok(IsNull::Yes),
            Some(text) => {
                out.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

impl ToSqlHolder {
    fn as_to_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            // A typed NULL: `Option::<&str>::None` renders as a text NULL bind.
            Self::Null => &Option::<&str>::None,
            Self::Bool(b) => b,
            Self::Int(n) => n,
            Self::Text(s) => s,
            Self::Untyped(u) => u,
        }
    }

    /// The type this holder DECLARES. `UNKNOWN` is not a fallback here - it is the
    /// positive statement "I am declaring nothing", which is what makes the
    /// server-side coercion happen.
    fn declared_type(&self) -> Type {
        match self {
            Self::Null | Self::Text(_) => Type::TEXT,
            Self::Bool(_) => Type::BOOL,
            Self::Int(_) => Type::INT8,
            Self::Untyped(_) => Type::UNKNOWN,
        }
    }
}

fn to_holder(bind: &Bind) -> Result<ToSqlHolder, DbError> {
    // The wildcard arm is NOT optional: `Bind` is `#[non_exhaustive]`, so an
    // out-of-crate match cannot be exhaustive and the compiler will never warn a
    // driver that a variant arrived unhandled. That is precisely how
    // `Bind::Inferred` went missing here - the enum grew, this file did not, the
    // build stayed green, and every inferred parameter failed at RUNTIME with
    // "unsupported bind variant". The PostgreSQL backend maps every `BindValue` to
    // `Inferred`, so that was the main path rather than an edge.
    //
    // Since the compiler cannot hold this seam, the driver conformance suite has to:
    // `zeroship_migrate_backend::driver::conformance` invariant 3 binds a declared
    // and an inferred parameter, separately and mixed, against a live server. A
    // driver that adds a variant here without running it is back to a green build
    // over a broken bind path.
    //
    // That suite is now pointed at THIS driver, from
    // `tests/compio_pg_conformance.rs`. Measured 2026-09-04 against PostgreSQL
    // 18.6: deleting the `Bind::Inferred` arm below reproduces the historic bug
    // exactly, and the target reports it as
    //   check "bind-inference-semantics": all-inferred INSERT with a
    //   text-to-timestamp coercion failed: unsupported bind variant
    match bind {
        Bind::Null => Ok(ToSqlHolder::Null),
        Bind::Bool(b) => Ok(ToSqlHolder::Bool(*b)),
        Bind::Int(n) => Ok(ToSqlHolder::Int(*n)),
        // Decimal carried as text - PG infers the numeric target from context.
        Bind::Decimal(s) => Ok(ToSqlHolder::Text(s.clone())),
        Bind::Text(s) => Ok(ToSqlHolder::Text(s.clone())),
        Bind::Inferred(v) => Ok(ToSqlHolder::Untyped(Untyped(v.clone()))),
        _ => unsupported_bind_to_holder(bind),
    }
}

fn unsupported_bind_to_holder<T>(_: &T) -> Result<ToSqlHolder, DbError> {
    Err(DbError::message("unsupported bind variant"))
}

fn bind_holders(params: &[Bind]) -> Result<Vec<ToSqlHolder>, DbError> {
    params.iter().map(to_holder).collect()
}

/// Pair every holder with the type it declares, the shape `execute_typed` /
/// `query_typed` take. The engine's per-VALUE inference is only expressible here:
/// the untyped `execute`/`query` pair declares one type per parameter from the
/// `ToSql` impl, with no way to say "declare nothing for this one".
fn holder_params(holders: &[ToSqlHolder]) -> Vec<(&(dyn ToSql + Sync), Type)> {
    holders
        .iter()
        .map(|h| (h.as_to_sql(), h.declared_type()))
        .collect()
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

/// A text-family base type - decoded as `String` / `TextArray` element.
fn is_text_family(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::TEXT | Type::NAME | Type::VARCHAR | Type::BPCHAR | Type::UNKNOWN
    )
}

/// Decode ONE `compio_postgres::Row` cell into a neutral [`Value`], reproducing the
/// in-tree adapter's classification byte-for-byte: text-family / `"char"` /
/// `to_char`-timestamps -> `Text`; int2/int4/int8/oid -> `Int`; bool -> `Bool`;
/// text[] -> `TextArray` (element-NULLs preserved); SQL NULL -> `Null`.
fn cell_to_value(row: &PgRow, idx: usize, ty: &Type) -> Result<Value, DbError> {
    let base = resolve_domain(ty);

    // Arrays first: `text[]` (and any `Kind::Array` whose element is text-family)
    // -> `TextArray`. Also catches `information_schema` array domains.
    //
    // Nested `if`s rather than an `if let ... && ...` chain: this crate is edition
    // 2021 and let chains need 2024. Same evaluation order, same short circuit.
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

/// `compio_postgres::Row -> driver::Row` - iterate columns, decode each cell.
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

/// The native, compio-postgres [`SqlSession`] impl - one forward per verb,
/// mapping binds, rows and errors through the neutral seam.
///
/// The peer producer is `NapiHostSession`
/// (`crates/zeroship-migrate-node/src/session.rs`), which answers the same four
/// verbs over the JS `pg` host. One body of engine code runs over either, so the
/// SQL issued, the transaction boundaries and the decoded [`Value`] cells have
/// to come out the same.
///
/// THIS DOC SAID "NOTHING IN THE TREE HOLDS THAT" UNTIL 2026-09-04, and for the
/// half about THIS driver it is no longer true.
/// `zeroship_migrate_backend::driver::conformance` is the suite built for it, and
/// it was driven only from `crates/zeroship-migrate/tests/` - `pg_engine/`
/// `pg_conformance.rs` against the harness `PgDevSession`, `mysql_engine/`
/// `mysql_conformance.rs` against `MysqlDevSession`. Both are TEST drivers, so the
/// one that ships was the one nothing conformed. `tests/compio_pg_conformance.rs`
/// now runs the full suite against `CompioPgSession` on a live server, and proves
/// it reached THIS session rather than some other: `pg_my_temp_schema()` is 0
/// before the run and non-zero after, on the same backend pid the borrowed
/// `compio_postgres::Client` reports. That closes the gap `to_holder`'s comment
/// above names when it says the compiler cannot hold this seam and the conformance
/// suite has to.
///
/// The OTHER half stands: `NapiHostSession` still has no conformance target, so
/// nothing holds the two producers to the same behaviour.
///
/// This doc said "identical to the platform's in-tree `PgSession` impl" until
/// 2026-09-04. No such type exists or has existed; see the module header.
impl SqlSession for CompioPgSession {
    async fn batch(&self, sql: &str) -> Result<(), DbError> {
        self.client
            .batch_execute(sql)
            .await
            .map_err(|e| to_db_error(&e))
    }

    // All three bind paths go through the `*_typed` entry points rather than
    // `execute`/`query`, because those declare a type per parameter off the `ToSql`
    // impl and give the caller no way to decline. Declining is exactly what
    // `Bind::Inferred` needs, and it is a property of the VALUE, so a statement may
    // mix an inferred instant with a typed key - which the previous whole-statement
    // `exec_text` could not express and which `holder_params` now carries.
    async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError> {
        let holders = bind_holders(binds)?;
        let params = holder_params(&holders);
        self.client
            .execute_typed(sql, &params)
            .await
            .map_err(|e| to_db_error(&e))
    }

    async fn query(&self, sql: &str, binds: &[Bind]) -> Result<Vec<Row>, DbError> {
        let holders = bind_holders(binds)?;
        let params = holder_params(&holders);
        let rows = self
            .client
            .query_typed(sql, &params)
            .await
            .map_err(|e| to_db_error(&e))?;
        rows.iter().map(row_to_neutral).collect()
    }

    async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError> {
        let holders = bind_holders(binds)?;
        let params = holder_params(&holders);
        let rows = self
            .client
            .query_typed(sql, &params)
            .await
            .map_err(|e| to_db_error(&e))?;
        match rows.as_slice() {
            [row] => row_to_neutral(row),
            other => Err(DbError::message(format!(
                "query_one expected exactly one row, got {}",
                other.len()
            ))),
        }
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
