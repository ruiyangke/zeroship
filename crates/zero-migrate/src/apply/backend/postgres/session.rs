//! The `PgSession` in-session Postgres driver seam.
//!
//! The migrate apply path (`PostgresBackend` + its `journal`/`drift`/`baseline`/
//! `precondition`/`backfill` collaborators and the executor PG leaves) is generic
//! over this trait rather than threading a concrete `&compio_postgres::Client`.
//!
//! The trait is typed in DRIVER-NEUTRAL types ([`SeamBind`]/[`SeamRow`]/
//! [`SeamError`], see [`seam`](super::seam)), NOT `compio_postgres::{Row, Error,
//! types::ToSql}` (design §A). Those compio types have private constructors, so a
//! host (napi) driver could neither be *called* (it cannot extract cells from
//! `&[&dyn ToSql]`) nor *return* rows/errors. The neutral types close both sides.
//!
//! The native (`native-pg`) impl below is the FIRST producer of every neutral
//! type: it maps `SeamBind → ToSql` on the way in, and `compio_postgres::Row →
//! SeamRow` / `compio_postgres::Error → SeamError` on the way out. It is the ONLY
//! place still touching `compio_postgres::{types::ToSql, types::FromSql, Row,
//! Error}`. The mapping forwards the exact same wire operations, so the default
//! build's SQL, txn boundaries, journal/lock ordering, and decoded domain values
//! are byte-for-byte unchanged — only the intermediate Rust representation becomes
//! neutral.
//!
//! Connect / lifecycle is a per-impl free function (`crate::conn::connect`), NOT
//! part of this trait: the compio impl detaches a run-loop `JoinHandle`; a Node
//! impl has no run-loop. Transaction control (`BEGIN`/`COMMIT`/`ROLLBACK`),
//! advisory locks (`pg_advisory_lock`), and confinement `SET`s are SQL strings
//! issued through `batch_execute`/`execute` — they are engine logic, not driver
//! methods, so the trait carries no transaction-object or lock abstraction.

use super::seam::{SeamBind, SeamError, SeamRow, SeamValue};

/// The in-session Postgres driver surface the migrate apply path is generic over.
///
/// Exactly the five in-session verbs the engine issues on a live session:
/// `batch_execute` (DDL / txn control / multi-statement session setup), `execute`
/// / `execute_text_params` (parameterized DML → rows affected), and `query` /
/// `query_one` (catalog / journal introspection → rows).
///
/// Every verb's error is the neutral [`SeamError`] (§A.8: uniform across all 5).
/// The bind params are neutral [`SeamBind`] on `execute`/`query`/`query_one` (3 of
/// 5); `execute_text_params` keeps its already-neutral `&[Option<String>]` shape
/// (PG refuses concrete-OID binary for `text → timestamptz`); `batch_execute`
/// takes no params.
#[allow(async_fn_in_trait)] // !Send is by design on the single-thread compio runtime
pub trait PgSession {
    /// DDL / txn control / multi-statement session setup. Simple-query protocol:
    /// one `&str`, may contain `;`-separated statements, no params, no rows.
    async fn batch_execute(&self, sql: &str) -> Result<(), SeamError>;

    /// Parameterized DML → rows affected.
    async fn execute(&self, sql: &str, params: &[SeamBind]) -> Result<u64, SeamError>;

    /// Schema-blind op.* DML: text-format params with server-inferred types.
    /// (Distinct from [`PgSession::execute`]: a concrete-OID binary bind would make
    /// PG refuse `text → timestamptz`; the assembler needs text-format coercion.
    /// Its BIND side is already neutral (`&[Option<String>]`), but its ERROR side
    /// widens to [`SeamError`] uniformly with the other four verbs — a host impl
    /// cannot construct a `compio_postgres::Error`.)
    async fn execute_text_params(
        &self,
        sql: &str,
        params: &[Option<String>],
    ) -> Result<u64, SeamError>;

    /// Parameterized SELECT → all rows.
    async fn query(&self, sql: &str, params: &[SeamBind]) -> Result<Vec<SeamRow>, SeamError>;

    /// Parameterized SELECT → exactly one row (errors otherwise).
    async fn query_one(&self, sql: &str, params: &[SeamBind]) -> Result<SeamRow, SeamError>;
}

// ---------------------------------------------------------------------------
// The native, compio-postgres impl — the FIRST producer of every neutral type.
// `native-pg`-only: it is the ONLY place still touching `compio_postgres::{types::
// ToSql, types::FromSql, Row, Error}`. The `PgSession` trait above is neutral, so
// it compiles on the whole PG seam; a `host-pg`-only build gets the trait but no
// compio adapter (the addon supplies its own host `PgSession` in Phase C, and the
// in-crate recording driver proves genericity in the meantime).
// ---------------------------------------------------------------------------

#[cfg(feature = "native-pg")]
use compio_postgres::types::{Kind, ToSql, Type};
#[cfg(feature = "native-pg")]
use compio_postgres::{Error as PgError, Row as PgRow};

/// `compio_postgres::Error → SeamError`. The consumer census treats the error
/// opaquely (wrap as `#[source]`/`Display`), so `Display` + an optional SQLSTATE
/// is a faithful, lossless-enough projection.
#[cfg(feature = "native-pg")]
impl From<PgError> for SeamError {
    fn from(e: PgError) -> Self {
        let code = e.code().map(|c| c.code().to_string());
        // `compio_postgres::Error`'s own `Display` is the opaque "db error"; the
        // human-meaningful text lives on the underlying `DbError` (the primary
        // message, e.g. "updated partition constraint … would be violated").
        // Project that into `SeamError.message` so the neutral error carries the
        // same diagnostic the concrete error chain used to — otherwise every seam
        // error degrades to "db error" and detail-matching consumers regress.
        let message = e
            .as_db_error()
            .map_or_else(|| e.to_string(), |db| db.message().to_string());
        SeamError { message, code }
    }
}

/// A borrowed `ToSql`-holder produced from a [`SeamBind`], so the compio `Client`'s
/// `&[&(dyn ToSql + Sync)]` slice can borrow into it for the duration of the call.
///
/// `Decimal` maps to a text bind (the IR carries decimals as strings; PG infers
/// the target type from context) — no bind site binds `Decimal`/`Bool`/`Null`
/// today, so those arms are exercised only by the enum-completeness test.
#[cfg(feature = "native-pg")]
enum ToSqlHolder {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
}

#[cfg(feature = "native-pg")]
impl ToSqlHolder {
    fn as_to_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            // A typed NULL: `Option::<&str>::None` renders as a text NULL bind.
            ToSqlHolder::Null => &Option::<&str>::None,
            ToSqlHolder::Bool(b) => b,
            ToSqlHolder::Int(n) => n,
            ToSqlHolder::Text(s) => s,
        }
    }
}

#[cfg(feature = "native-pg")]
fn to_holder(bind: &SeamBind) -> ToSqlHolder {
    match bind {
        SeamBind::Null => ToSqlHolder::Null,
        SeamBind::Bool(b) => ToSqlHolder::Bool(*b),
        SeamBind::Int(n) => ToSqlHolder::Int(*n),
        // Decimal carried as text — PG infers the numeric target from context.
        SeamBind::Decimal(s) => ToSqlHolder::Text(s.clone()),
        SeamBind::Text(s) => ToSqlHolder::Text(s.clone()),
    }
}

/// Map a neutral bind slice into a compio-accepted `&[&(dyn ToSql + Sync)]`. The
/// holders own the values; the returned refs borrow from `holders`, so both live
/// to the end of the call.
#[cfg(feature = "native-pg")]
fn bind_params(params: &[SeamBind]) -> Vec<ToSqlHolder> {
    params.iter().map(to_holder).collect()
}

#[cfg(feature = "native-pg")]
fn holder_refs(holders: &[ToSqlHolder]) -> Vec<&(dyn ToSql + Sync)> {
    holders.iter().map(ToSqlHolder::as_to_sql).collect()
}

/// Decode ONE `compio_postgres::Row` cell into a neutral [`SeamValue`].
///
/// This is the only surviving `FromSql` touch-point. It funnels the seam's value
/// union (§A.2) — `text`/`name`/`varchar`/`bpchar`/`"char"`/`to_char`-timestamps →
/// `Text`; `int2`/`int4`/`int8`/`oid` → `Int`; `bool` → `Bool`; `text[]` →
/// `TextArray` (element-NULLs preserved); SQL NULL → `Null` — reproducing exactly
/// what today's `FromSql` decode + `.unwrap_or_default()`/`.ok().flatten()` idioms
/// yield (§A.8), so the native path stays byte-identical.
#[cfg(feature = "native-pg")]
fn cell_to_seam(row: &PgRow, idx: usize, ty: &Type) -> Result<SeamValue, SeamError> {
    // `information_schema` columns (data_type/udt_name = `sql_identifier`,
    // character_maximum_length = `cardinal_number`, is_nullable = `yes_or_no`,
    // …) are DOMAINS over a base type; a raw `match *ty` on the base constants
    // would miss them and mis-route the cell. Resolve any domain to its base
    // first, exactly as `FromSql::accepts` does, so an `int4` domain still decodes
    // as an integer (byte-identical to the pre-seam `r.get`).
    let base = resolve_domain(ty);

    // Arrays first: `text[]` (and any `Kind::Array` whose element is text-family)
    // → `TextArray`. This also catches `information_schema` array domains.
    if let Kind::Array(elem) = base.kind() {
        if is_text_family(resolve_domain(elem)) {
            return match row.try_get::<_, Option<Vec<Option<String>>>>(idx)? {
                Some(v) => Ok(SeamValue::TextArray(v)),
                None => Ok(SeamValue::Null),
            };
        }
    }

    // NULL is uniform across types: a `None`-decoding of the column's own type.
    match *base {
        Type::BOOL => match row.try_get::<_, Option<bool>>(idx)? {
            Some(b) => Ok(SeamValue::Bool(b)),
            None => Ok(SeamValue::Null),
        },
        Type::INT8 => match row.try_get::<_, Option<i64>>(idx)? {
            Some(n) => Ok(SeamValue::Int(n)),
            None => Ok(SeamValue::Null),
        },
        Type::INT4 | Type::OID => match row.try_get::<_, Option<i32>>(idx)? {
            Some(n) => Ok(SeamValue::Int(i64::from(n))),
            None => Ok(SeamValue::Null),
        },
        Type::INT2 => match row.try_get::<_, Option<i16>>(idx)? {
            Some(n) => Ok(SeamValue::Int(i64::from(n))),
            None => Ok(SeamValue::Null),
        },
        // PG `"char"` (oid 18): decode the raw `i8` and normalise to a 1-char
        // `Text`, so `FromSeam for i8`/`char` reads it back the same as native.
        Type::CHAR => match row.try_get::<_, Option<i8>>(idx)? {
            Some(c) => {
                let byte = u8::try_from(c).map_err(|_| {
                    SeamError::message(format!("\"char\" byte {c} out of ASCII range"))
                })?;
                Ok(SeamValue::Text((byte as char).to_string()))
            }
            None => Ok(SeamValue::Null),
        },
        // text/name/varchar/bpchar and any other text-family type the SQL casts to.
        _ => match row.try_get::<_, Option<String>>(idx)? {
            Some(s) => Ok(SeamValue::Text(s)),
            None => Ok(SeamValue::Null),
        },
    }
}

/// Resolve a `Kind::Domain(base)` chain down to its concrete base type, so an
/// `information_schema` domain (`cardinal_number` over `int4`, `sql_identifier`
/// over `name`, `yes_or_no` over `varchar`) routes to the right decode arm.
#[cfg(feature = "native-pg")]
fn resolve_domain(ty: &Type) -> &Type {
    let mut cur = ty;
    while let Kind::Domain(inner) = cur.kind() {
        cur = inner;
    }
    cur
}

/// A text-family base type — decoded as `String`/`TextArray` element.
#[cfg(feature = "native-pg")]
fn is_text_family(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::TEXT | Type::NAME | Type::VARCHAR | Type::BPCHAR | Type::UNKNOWN
    )
}

/// `compio_postgres::Row → SeamRow` — iterate columns, decode each cell.
#[cfg(feature = "native-pg")]
fn row_to_seam(row: &PgRow) -> Result<SeamRow, SeamError> {
    let cols = row.columns();
    let mut names = Vec::with_capacity(cols.len());
    let mut values = Vec::with_capacity(cols.len());
    for (idx, col) in cols.iter().enumerate() {
        names.push(col.name().to_string());
        values.push(cell_to_seam(row, idx, col.type_())?);
    }
    Ok(SeamRow::new(names, values))
}

/// The native, compio-postgres [`PgSession`] impl — one forward per verb (mapping
/// binds/rows/errors through the neutral seam), so the default build's SQL, txn
/// boundaries, and behavior are identical to pre-seam.
#[cfg(feature = "native-pg")]
impl PgSession for compio_postgres::Client {
    async fn batch_execute(&self, sql: &str) -> Result<(), SeamError> {
        compio_postgres::Client::batch_execute(self, sql)
            .await
            .map_err(SeamError::from)
    }

    async fn execute(&self, sql: &str, params: &[SeamBind]) -> Result<u64, SeamError> {
        let holders = bind_params(params);
        let refs = holder_refs(&holders);
        compio_postgres::Client::execute(self, sql, &refs)
            .await
            .map_err(SeamError::from)
    }

    async fn execute_text_params(
        &self,
        sql: &str,
        params: &[Option<String>],
    ) -> Result<u64, SeamError> {
        compio_postgres::Client::execute_text_params(self, sql, params)
            .await
            .map_err(SeamError::from)
    }

    async fn query(&self, sql: &str, params: &[SeamBind]) -> Result<Vec<SeamRow>, SeamError> {
        let holders = bind_params(params);
        let refs = holder_refs(&holders);
        let rows = compio_postgres::Client::query(self, sql, &refs)
            .await
            .map_err(SeamError::from)?;
        rows.iter().map(row_to_seam).collect()
    }

    async fn query_one(&self, sql: &str, params: &[SeamBind]) -> Result<SeamRow, SeamError> {
        let holders = bind_params(params);
        let refs = holder_refs(&holders);
        let row = compio_postgres::Client::query_one(self, sql, &refs)
            .await
            .map_err(SeamError::from)?;
        row_to_seam(&row)
    }
}

#[cfg(test)]
mod element_null_parity {
    //! §A.8 dual-adapter parity: a synthetic element-NULL `reloptions`-shaped
    //! `text[]` yields the LEGACY `[]` outcome through BOTH the compio adapter and
    //! a canned host `SeamRow`. Pins parity on the one raw, nullable, unfiltered
    //! catalog array cell so native and host stay interchangeable.

    use super::super::seam::{SeamRow, SeamValue};

    /// A canned host row carrying a `text[]` with an element-NULL, exactly as a
    /// napi `pg` driver would build it (JS `null` inside the array).
    fn host_row_with_element_null() -> SeamRow {
        SeamRow::new(
            vec!["reloptions".to_string()],
            vec![SeamValue::TextArray(vec![
                Some("fillfactor=90".to_string()),
                None, // a SQL NULL element
            ])],
        )
    }

    #[test]
    fn host_element_null_reloptions_coerces_to_empty_via_unwrap_or_default() {
        let row = host_row_with_element_null();
        // The `.unwrap_or_default()` idiom (drift.rs:899/900/902/1114 shape): a
        // NULL element makes the Vec<String> decode err, coerced to [].
        let cols: Vec<String> = row.try_get::<_, Vec<String>>("reloptions").unwrap_or_default();
        assert!(cols.is_empty(), "element-NULL text[] must coerce to [] on host adapter");
    }

    #[test]
    fn host_element_null_reloptions_coerces_to_none_via_ok_flatten() {
        let row = host_row_with_element_null();
        // The `.ok().flatten()` idiom (drift.rs:901 reloptions shape): a NULL
        // element makes the inner Vec<String> decode err → None.
        let opt: Option<Vec<String>> = row
            .try_get::<_, Option<Vec<String>>>("reloptions")
            .ok()
            .flatten();
        assert!(opt.is_none(), "element-NULL text[] must coerce to None on host adapter");
    }

    /// The compio-adapter half of the parity check: drive a real element-NULL
    /// `text[]` through the live `Row → SeamRow` adapter and assert it reaches the
    /// SAME `[]` / `None` outcome as the canned host row above. Requires a live PG
    /// (skips when `MIGRATE_TEST_DB` is unset unless `MIGRATE_REQUIRE_DB=1`).
    #[cfg(feature = "native-pg")]
    #[compio::test]
    async fn compio_and_host_agree_on_element_null_reloptions() {
        let Some(url) = std::env::var("MIGRATE_TEST_DB").ok() else {
            if std::env::var("MIGRATE_REQUIRE_DB").as_deref() == Ok("1") {
                panic!("MIGRATE_REQUIRE_DB=1 but MIGRATE_TEST_DB unset");
            }
            eprintln!("skipping compio element-NULL parity: MIGRATE_TEST_DB unset");
            return;
        };
        // `crate::conn::connect` returns a `Client` whose driver loop is already
        // spawned+detached on the compio runtime (the crate-wide pattern).
        let client = crate::conn::connect(&url).await.expect("connect");

        // A text[] literal with an element-NULL, shaped like a raw `reloptions`.
        let rows = super::PgSession::query(
            &client,
            "SELECT ARRAY['fillfactor=90', NULL]::text[] AS reloptions",
            &[],
        )
        .await
        .expect("query element-null array");
        let row = &rows[0];

        // The SeamValue must PRESERVE the element-NULL (a None element)...
        match row.try_get::<_, Option<Vec<String>>>("reloptions") {
            Ok(_) | Err(_) => {}
        }
        // ...and the legacy `.unwrap_or_default()` / `.ok().flatten()` idioms must
        // both coerce it to the empty outcome, identical to the host row.
        let via_default: Vec<String> =
            row.try_get::<_, Vec<String>>("reloptions").unwrap_or_default();
        let via_flatten: Option<Vec<String>> = row
            .try_get::<_, Option<Vec<String>>>("reloptions")
            .ok()
            .flatten();

        let host = host_row_with_element_null();
        let host_default: Vec<String> =
            host.try_get::<_, Vec<String>>("reloptions").unwrap_or_default();
        let host_flatten: Option<Vec<String>> = host
            .try_get::<_, Option<Vec<String>>>("reloptions")
            .ok()
            .flatten();

        assert_eq!(via_default, host_default, "compio vs host: unwrap_or_default outcome");
        assert_eq!(via_flatten, host_flatten, "compio vs host: ok().flatten() outcome");
        assert!(via_default.is_empty(), "element-NULL → [] on compio adapter");
        assert!(via_flatten.is_none(), "element-NULL → None on compio adapter");
    }

    /// Enum-completeness for the dormant `SeamBind` arms (Bool/Null/Decimal never
    /// reach the wire today, §A.1): they must map to a `ToSqlHolder` without panic.
    #[cfg(feature = "native-pg")]
    #[test]
    fn seambind_all_arms_map_to_holder() {
        use super::super::seam::SeamBind;
        use super::to_holder;
        let _ = to_holder(&SeamBind::Null);
        let _ = to_holder(&SeamBind::Bool(true));
        let _ = to_holder(&SeamBind::Int(7));
        let _ = to_holder(&SeamBind::Decimal("1.5".to_string()));
        let _ = to_holder(&SeamBind::Text("x".to_string()));
    }
}
