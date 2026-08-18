//! Shared integration-test policy fixtures and live-Postgres support.
//!
//! [`PgDevSession`] is a TEST-ONLY [`zero_migrate::driver::SqlSession`] implementation
//! backed by the BLOCKING `postgres` crate. It lets the in-crate Rust tests drive the
//! SHIPPED generic PG apply path — `PostgresBackend<PgDevSession>`, the `<D: SqlSession>`
//! journal/drift/precondition/baseline free functions, `ops::status` — against a live
//! Postgres through the SAME driver seam the production napi/Node `pg` host
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

/// The live-MySQL sibling of everything below: `MysqlDevSession`, `DatabaseGuard`,
/// and the `ZERO_MIGRATE_MYSQL_URL` gate. It shares this module's
/// [`announce_live_db_skip`] / [`live_db_required`] discipline, so a missing MySQL
/// DSN skips loudly and fails under `ZERO_MIGRATE_REQUIRE_LIVE_DB`.
#[macro_use]
pub mod mysql;

/// The RENAME CARRIER INVENTORY shared by `rename_carrier_sweep_pg` and
/// `rename_carrier_sweep_sqlite`: every field of a `TableSnapshot` that can spell a
/// column name, enumerated by exhaustive destructuring so a new field cannot be added
/// without classifying it.
pub mod carriers;

/// The FIELD PROBE inventory shared by `structural_equality_field_sensitivity.rs` and
/// `support::model_equivalence`: one mutation per field of every snapshot type, so a new
/// field is a compile error until both consumers cover it.
pub mod field_probes;

/// The behaviour-preservation kernel for the neutral `schema_model`: the lossless
/// round trip and the named-comparator equivalence, shared by the PostgreSQL and MySQL
/// legs so the two cannot measure different things.
pub mod model_equivalence;

/// The step 4 consumer 3 corpus and its reduction to golden lines, shared by the
/// capture binary that recorded the golden from the OLD path and by
/// `gen_types_field_defs_from_the_fold.rs`, which compares against it. Two binaries so
/// a capture harness can never re-bless the file the comparison reads.
pub mod field_defs_corpus;

use std::cell::{Cell, RefCell};

use bytes::BytesMut;
use postgres::types::{Format, IsNull, Kind, ToSql, Type};
use postgres::{Client, NoTls, Row as PgRow};

use zero_migrate::driver::{Bind, DbError, Row, SqlSession, Value};
use zero_migrate::{effective_policy_from_charter_toml, EffectivePolicy};

pub const CONFINED_CHARTER_TOML: &str = r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }

[[grant]]
key = "schema.rename"
value = true
scope = { include = ["app"] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
  { name = "updated_at", type = "timestamptz", nullable = false },
  { name = "created_by", type = "text",        nullable = true  },
  { name = "updated_by", type = "text",        nullable = true  },
  { name = "version",    type = "integer",     nullable = false },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [
  { name = "ix_deleted_at", columns = ["deleted_at"] },
  { name = "ix_updated_at", columns = ["updated_at"] },
  { name = "ix_created_by", columns = ["created_by"] },
]
"#;

#[must_use]
pub fn confined_charter() -> EffectivePolicy {
    effective_policy_from_charter_toml(CONFINED_CHARTER_TOML)
        .expect("explicit confined test charter composes")
}

#[must_use]
pub fn no_inject(schema: &str) -> EffectivePolicy {
    let charter_toml = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "schema.rename"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#
    );
    effective_policy_from_charter_toml(&charter_toml)
        .expect("explicit no-inject test charter composes")
}

/// [`no_inject`] plus every vendor capability grant, for tests that assert a
/// privileged op RENDERS.
///
/// A vendor op's authority at lower is the charter's capability grant, so a test that
/// expects `createSchema` or a raw view body to lower has to author the grant - a
/// widened `SchemaScope` answers which schemas the migration may touch, not which
/// privileged primitives it may emit. The extension allowlist covers the names the
/// vendor fixtures create.
#[must_use]
pub fn operator_charter(schema: &str) -> EffectivePolicy {
    let charter_toml = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "schema.rename"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[grant]]
key = "access.role"
value = true
scope = "all"

[[grant]]
key = "access.grant"
value = true
scope = "all"

[[grant]]
key = "access.rls"
value = true
scope = "all"

[[grant]]
key = "access.policy"
value = true
scope = "all"

[[grant]]
key = "schema.create_schema"
value = true
scope = "all"

[[grant]]
key = "schema.partition"
value = true
scope = "all"

[[grant]]
key = "code.function"
value = true
scope = "all"

[[grant]]
key = "code.materialized_view"
value = true
scope = "all"

[[grant]]
key = "code.extension"
value = ["citext", "pgcrypto"]
scope = "all"

[[grant]]
key = "sql.raw"
value = true
scope = "all"

[[grant]]
key = "sql.raw_view_body"
value = true
scope = "all"
"#
    );
    effective_policy_from_charter_toml(&charter_toml)
        .expect("explicit operator test charter composes")
}

#[must_use]
pub fn no_inject_with_extensions(schema: &str, extensions: &[&str]) -> EffectivePolicy {
    let extensions = extensions
        .iter()
        .map(|extension| format!("{extension:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let charter_toml = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "schema.rename"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "code.extension"
value = [{extensions}]
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#
    );
    effective_policy_from_charter_toml(&charter_toml)
        .expect("explicit no-inject extension test charter composes")
}

/// The env var gating the live-Postgres tests. When unset, every live test skips
/// cleanly (so DB-free CI stays green); when set to a DSN, the suite runs against it.
pub const PG_URL_ENV: &str = "ZERO_MIGRATE_TEST_PG_URL";

/// The env var that turns a live-database skip into a test failure. Set it when a run
/// is EXPECTED to have a database (CI, a release gate, a local run you want proved) so
/// a missing DSN fails loudly instead of reporting green coverage that never ran.
pub const REQUIRE_LIVE_DB_ENV: &str = "ZERO_MIGRATE_REQUIRE_LIVE_DB";

/// Read the live-PG DSN from [`PG_URL_ENV`], or `None` when unset (→ skip).
///
/// Accepts either the libpq keyword form
/// (`host=… port=… user=… password=… dbname=…`) or a `postgres://…` URL — the
/// `postgres` crate parses both.
#[must_use]
pub fn pg_url() -> Option<String> {
    std::env::var(PG_URL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Whether [`REQUIRE_LIVE_DB_ENV`] demands that live-database tests actually run.
///
/// Anything but unset, empty, `0`, `false` or `no` counts as demanding a live run.
#[must_use]
pub fn live_db_required() -> bool {
    std::env::var(REQUIRE_LIVE_DB_ENV).is_ok_and(|raw| {
        let flag = raw.trim().to_ascii_lowercase();
        !matches!(flag.as_str(), "" | "0" | "false" | "no")
    })
}

/// Announce that this test binary is skipping its live-database coverage, or panic
/// when [`REQUIRE_LIVE_DB_ENV`] says a live run was expected.
///
/// A skipped live suite used to be INVISIBLE rather than merely quiet: the early
/// return still counts as a pass, so `cargo test` printed the same `30 passed` a
/// genuine run prints, and the `eprintln!` explaining why was swallowed by libtest's
/// output capture (which only intercepts the `print!`/`eprint!` macros). Writing to
/// the process stderr handle bypasses that capture, so the notice survives a default
/// `cargo test` and a database-free run reads differently from a real one.
///
/// The notice fires once per test binary: every gated test in the binary shares the
/// same cause, so repeating it per test would bury the summary it sits next to.
///
/// # Panics
/// Panics when [`REQUIRE_LIVE_DB_ENV`] is set and `env_var` is not, which fails the
/// calling test rather than passing it without coverage.
pub fn announce_live_db_skip(env_var: &str) {
    use std::io::Write as _;
    use std::sync::Once;

    assert!(
        !live_db_required(),
        "{REQUIRE_LIVE_DB_ENV} demands a live database but {env_var} is unset, so this \
         test has no coverage to offer; export a DSN or clear {REQUIRE_LIVE_DB_ENV}"
    );

    static NOTICE: Once = Once::new();
    NOTICE.call_once(|| {
        let banner = format!(
            "\n\
             ==================== LIVE-DATABASE COVERAGE SKIPPED ====================\n\
             {env_var} is unset, so the gated tests in this binary report \"ok\"\n\
             without touching a database. The passed count below says NOTHING about\n\
             live coverage. Export a DSN to run them for real, or set\n\
             {REQUIRE_LIVE_DB_ENV}=1 to turn this skip into a failure.\n\
             ========================================================================\n"
        );
        // Ignore a broken stderr: a closed pipe must not fail an otherwise fine test.
        let _ = std::io::stderr().write_all(banner.as_bytes());
    });
}

/// Yield the live-PG DSN, or announce the skip and return from the calling test when
/// [`PG_URL_ENV`] is unset.
#[macro_export]
macro_rules! skip_if_no_pg {
    () => {{
        match $crate::support::pg_url() {
            Some(url) => url,
            None => {
                $crate::support::announce_live_db_skip($crate::support::PG_URL_ENV);
                return;
            }
        }
    }};
}

/// How the next statement matching a needle reports back to the engine AFTER the
/// live server has already executed it.
///
/// The distinction from [`PgDevSession::fail_next_resolved_pending_contract_insert`]
/// is the ordering: that one refuses BEFORE the statement reaches the server, so
/// the server never does the work. These run the statement for real and only then
/// corrupt the reply, which is the only way to reproduce a server that recorded
/// something the client never learned about.
enum ReplyFaultKind {
    /// Report a driver error for a statement the server ran to completion.
    Error,
    /// Report a row whose named column carries text where the caller expects a
    /// boolean, so the caller's decode fails on a statement the server ran.
    UndecodableBool(String),
}

/// One armed reply fault: the SQL substring it waits for, and what it does.
struct ReplyFault {
    needle: String,
    kind: ReplyFaultKind,
}

/// A TEST-ONLY [`SqlSession`] over a blocking `postgres::Client`, pinned to ONE
/// connection (the seam's single-connection contract).
pub struct PgDevSession {
    client: RefCell<Client>,
    /// The DSN this session connected with, kept so cleanup that cannot use the pinned
    /// connection can open its own without the caller threading the string through.
    dsn: String,
    fail_next_resolved_pending_contract_insert: Cell<bool>,
    reply_fault: RefCell<Option<ReplyFault>>,
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
            dsn: dsn.to_string(),
            fail_next_resolved_pending_contract_insert: Cell::new(false),
            reply_fault: RefCell::new(None),
        }
    }

    /// Fail the next append of a resolved pending-contract event.
    ///
    /// This leaves all preceding database work untouched, which lets live tests
    /// reproduce a connection failure between resolver cleanup and its terminal
    /// tombstone without adding a production failpoint.
    pub fn fail_next_resolved_pending_contract_insert(&self) {
        self.fail_next_resolved_pending_contract_insert.set(true);
    }

    /// Let the live server run the next statement containing `needle`, then report
    /// a driver error for it.
    ///
    /// This is a client that lost the reply to work the server actually did. The
    /// server-side effect is real and outlives the error, which is what makes the
    /// resulting database state worth asserting against from a second session.
    pub fn fail_reply_after_running(&self, needle: &str) {
        *self.reply_fault.borrow_mut() = Some(ReplyFault {
            needle: needle.to_string(),
            kind: ReplyFaultKind::Error,
        });
    }

    /// Let the live server run the next statement containing `needle`, then report
    /// its `column` as text so a caller expecting a boolean fails to decode it.
    ///
    /// Same real server-side effect as [`Self::fail_reply_after_running`], reached
    /// through the decode branch instead of the transport branch.
    pub fn undecodable_bool_reply_after_running(&self, needle: &str, column: &str) {
        *self.reply_fault.borrow_mut() = Some(ReplyFault {
            needle: needle.to_string(),
            kind: ReplyFaultKind::UndecodableBool(column.to_string()),
        });
    }

    /// Take the armed reply fault if `sql` matches its needle. One-shot: a matched
    /// fault disarms itself so the compensating statement the engine sends next is
    /// never itself faulted.
    fn take_reply_fault(&self, sql: &str) -> Option<ReplyFaultKind> {
        let mut armed = self.reply_fault.borrow_mut();
        if armed
            .as_ref()
            .is_some_and(|fault| sql.contains(&fault.needle))
        {
            return armed.take().map(|fault| fault.kind);
        }
        None
    }
}

/// Drops the schemas a live-PG test created, on the way out of scope, whether the
/// test returned or unwound.
///
/// A test that calls `DROP SCHEMA` as its last statement only cleans up when it
/// reaches that statement. Every `assert!` between the CREATE and the DROP is a
/// point where a failing run abandons a schema on the server forever, and the leak
/// is silent: the run reports one failed test, not a database that now carries a
/// permanent `proj_<pid>_<nanos>_<n>` nobody will ever recognise or reclaim. A
/// single measured panic left exactly one schema behind (85 -> 86 on a server that
/// had already accumulated 85).
///
/// Constructed where the CREATE happens; the DROP then rides `Drop` and needs no
/// statement at the end of the test.
///
/// The DROP goes over the test's own pinned connection, NOT a fresh one. A test
/// that unwinds mid-transaction leaves that transaction open until its `Client` is
/// dropped, and `DROP SCHEMA ... CASCADE` from a second connection would block on
/// its locks for as long as the guard lives - a hung suite in place of a leaked
/// schema. Reusing the pinned connection sees those locks as its own.
///
/// `must_use` sits on the TYPE, not only on the functions that hand one back: an
/// `#[must_use]` on an `async fn` marks the future, which an `.await` consumes, so a
/// caller that awaits the guard and drops it on the spot would slip past it. On the
/// type, dropping the awaited value is itself the unused expression.
#[must_use = "bind the guard to a name; it drops the schemas when it falls out of scope"]
pub struct SchemaGuard<'a> {
    session: &'a PgDevSession,
    schemas: Vec<String>,
}

impl<'a> SchemaGuard<'a> {
    /// Take responsibility for dropping `schemas` when the guard goes out of scope.
    #[must_use = "the guard drops the schemas when it falls out of scope"]
    pub fn arm<I, S>(session: &'a PgDevSession, schemas: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            session,
            schemas: schemas.into_iter().map(Into::into).collect(),
        }
    }

    /// `DROP SCHEMA IF EXISTS` for every guarded schema, as one batch.
    fn drop_sql(&self) -> String {
        use std::fmt::Write as _;
        let mut sql = String::new();
        for schema in &self.schemas {
            let _ = write!(sql, "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE;");
        }
        sql
    }
}

impl Drop for SchemaGuard<'_> {
    fn drop(&mut self) {
        use std::io::Write as _;

        if self.schemas.is_empty() {
            return;
        }
        let sql = self.drop_sql();

        // `try_borrow_mut`, never `borrow_mut`: a panic raised while the seam holds
        // the client leaves the cell borrowed, and a panicking `Drop` during an
        // unwind aborts the process instead of failing the test.
        if let Ok(mut client) = self.session.client.try_borrow_mut() {
            // An unwind can land inside a failed transaction, where every later
            // statement is rejected until the block ends. Outside one this is a
            // warning and nothing else.
            let _ = client.batch_execute("ROLLBACK");
            if client.batch_execute(&sql).is_ok() {
                return;
            }
        }

        // The pinned connection could not do it. Timeouts are set first so a lock the
        // test still holds turns into an error we can report rather than a suite that
        // hangs here.
        let fallback = Client::connect(&self.session.dsn, NoTls).and_then(|mut client| {
            client.batch_execute(&format!(
                "SET lock_timeout = '5s'; SET statement_timeout = '15s'; {sql}"
            ))
        });
        if let Err(e) = fallback {
            // Straight to the process stderr handle: libtest captures the print
            // macros, and a cleanup failure that only shows on `--nocapture` is the
            // silent leak this guard exists to end. Never a panic - see above.
            let _ = writeln!(
                std::io::stderr(),
                "SchemaGuard: {} left behind on {}: {e}",
                self.schemas.join(", "),
                self.session.dsn
            );
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
    DbError { message, sqlstate }
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
fn pg_numeric_from_binary(raw: &[u8]) -> Result<String, Box<dyn std::error::Error + Sync + Send>> {
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
        if sql.contains(".schema_pending_contracts")
            && sql.contains("VALUES ('resolved'")
            && self
                .fail_next_resolved_pending_contract_insert
                .replace(false)
        {
            return Err(DbError::message(
                "test fault: resolved pending-contract append failed",
            ));
        }
        let holders = bind_holders(binds);
        let refs = holder_refs(&holders);
        let outcome = self
            .client
            .borrow_mut()
            .execute(sql, &refs)
            .map_err(|e| to_db_error(&e));
        match self.take_reply_fault(sql) {
            None => outcome,
            Some(_) => Err(DbError::message(
                "test fault: the server ran the statement and the client lost the reply",
            )),
        }
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
        let outcome = self
            .client
            .borrow_mut()
            .query_one(sql, &refs)
            .map_err(|e| to_db_error(&e));
        match self.take_reply_fault(sql) {
            None => row_to_neutral(&outcome?),
            Some(ReplyFaultKind::Error) => Err(DbError::message(
                "test fault: the server ran the statement and the client lost the reply",
            )),
            Some(ReplyFaultKind::UndecodableBool(column)) => Ok(Row::new(
                vec![column],
                vec![Value::Text("not a boolean".to_string())],
            )),
        }
    }
}

/// Drain a `RowIter` fully, returning (decoded rows, `rows_affected`). Used by
/// `exec_text` (ignores rows, reads the count).
fn drain_row_iter(mut iter: postgres::RowIter<'_>) -> Result<(Vec<Row>, u64), DbError> {
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
